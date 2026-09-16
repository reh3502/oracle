"""Run pinned, independently reviewed release questions against the real offline CLI.

No network or runtime source mutation. Requires the ignored complete saved corpus
and catalog. The fixture contains reviewed wiki excerpts under CC-BY-SA-3.0;
changing expectations requires a new source review, never copying query results.
"""
import argparse
import collections
import hashlib
import json
import subprocess
import tempfile
import time
from pathlib import Path

from wiki_source import Corpus

KINDS = {'toon', 'twisted', 'npc', 'floor', 'machine', 'mechanic',
         'trinket', 'item', 'event', 'topic'}
SUPPRESSED = 'The requested fact is not verified for a current answer.'


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def run(corpus_path, catalog_path, binary, fixture_path, output):
    fixture = json.loads(fixture_path.read_text())
    data = json.loads(catalog_path.read_text())
    corpus = Corpus.open(corpus_path)
    rows = {row['source']['id']: row for row in corpus.rows}
    sources = {source['id']: source for source in data['sources']}
    facts = {fact['id']: (entity, fact) for entity in data['entities'] for fact in entity['facts']}
    questions = fixture['questions']
    require(fixture['schema_version'] == 1, 'Unknown fixture version')
    require(len(questions) >= 40, 'At least forty reviewed questions required')
    require(len({q['id'] for q in questions}) == len(questions), 'Duplicate question IDs')
    normal_counts = collections.Counter(q['category'] for q in questions if q['case'] == 'normal')
    require(set(normal_counts) == KINDS, 'Normal questions must cover all ten categories')
    require(sum(q['case'] != 'normal' for q in questions) >= 10, 'Ten challenge questions required')
    require(data['sources'] == [r['source'] for r in corpus.rows], 'Catalog/corpus provenance drift')
    require(data['source_origin'] == fixture['source_origin'], 'Source origin changed')
    # This runs before publishing or querying: expectations are independently
    # fixed to reviewed source revisions, not inferred from observed answers.
    for fid, oracle in fixture['facts'].items():
        require(fid in facts, f'Missing reviewed fact {fid}')
        entity, fact = facts[fid]
        require(entity['id'] == oracle['entity_id'], f'Entity mapping drift: {fid}')
        require(fact['state'] == oracle['state'], f'Evidence state drift: {fid}')
        supports = oracle['reviewed_support']
        expected_citations = [{k: v for k, v in s.items() if k not in {'revision_id', 'content_sha256', 'revision_url'}} for s in supports]
        require(fact['citations'] == expected_citations, f'Reviewed support drift: {fid}')
        for support in supports:
            source = rows[support['source_id']]
            require(support['revision_url'] == fixture['source_origin'] + '/index.php?oldid=' + str(support['revision_id']), f'Invalid revision URL: {fid}')
            require(source['source']['revision_id'] == support['revision_id'], f'Revision drift: {fid}')
            require(source['source']['content_sha256'] == support['content_sha256'], f'Content hash drift: {fid}')
            require(support['quote'] in source['raw'], f'Excerpt not in saved source: {fid}')
        if oracle['state'] == 'supported':
            require('value' in oracle or oracle.get('text_contains'), f'Missing independent semantic oracle: {fid}')
        if 'value' in oracle:
            require(fact['value'] == oracle['value'], f'Reviewed value mismatch: {fid}')
        for text in oracle.get('text_contains', []):
            require(text in fact['text'], f'Reviewed semantic constraint missing: {fid}: {text}')
    now = max(s['validated_at_ms'] for s in data['sources']) + 1000
    output.mkdir(parents=True, exist_ok=True)
    results = []

    def cli(*args):
        return subprocess.run([str(binary), *map(str, args)], text=True, capture_output=True, timeout=60)

    with tempfile.TemporaryDirectory(prefix='store-', dir=output) as temporary:
        store = Path(temporary) / 'store'
        result = cli('publish', '--store', store, '--catalog', catalog_path)
        require(result.returncode == 0, 'Isolated publication failed: ' + result.stderr[:1000])
        publication = json.loads(result.stdout)
        for q in questions:
            started = time.monotonic()
            response = None
            error = None
            try:
                at = q.get('now_ms', now + q.get('time_offset_ms', 0))
                result = cli('query', '--store', store, '--input', json.dumps(q['request']), '--now-ms', at)
                require(result.returncode == 0, 'CLI rejected request: ' + result.stderr[:1000])
                response = json.loads(result.stdout)
                require(response['snapshot_id'] == publication['snapshot_id'], 'Snapshot changed')
                require(response['status'] == q['status'], f"Expected status {q['status']}, got {response['status']}")
                actual_ids = [fid for block in response['answer_blocks'] for fid in block['fact_ids']]
                require(sorted(actual_ids) == sorted(q['fact_ids']), f'Unexpected factual claims: {actual_ids}')
                require(set(q.get('candidate_ids', [])) <= {c['id'] for c in response['candidates']}, 'Missing disambiguation candidates')
                used_sources = set()
                for block in response['answer_blocks']:
                    require(len(block['fact_ids']) == 1, 'Unexpected combined claim')
                    fid = block['fact_ids'][0]
                    oracle = fixture['facts'][fid]
                    entity, fact = facts[fid]
                    require(block['entity_id'] == oracle['entity_id'], 'Wrong entity')
                    require(block['state'] == q.get('expected_state', oracle['state']), 'Wrong evidence state')
                    for key in ['key', 'unit', 'conditions', 'citations']:
                        require(block[key] == fact[key], f'{fid}: {key} changed or omitted')
                    expected_source_ids = {c['source_id'] for c in fact['citations']}
                    require(set(block['source_ids']) == expected_source_ids, 'Wrong claim/source mapping')
                    used_sources.update(expected_source_ids)
                    if q.get('suppressed') or fid in q.get('suppressed_fact_ids', []):
                        require(block['value'] is None and block['text'] == SUPPRESSED, 'Unverified claim was exposed')
                        require(block['warnings'], 'Suppressed claim lacks explanation')
                    else:
                        require(block['state'] == oracle['state'] == 'supported', 'Unqualified evidence state')
                        require(block['value'] == fact['value'] and block['text'] == fact['text'], 'Added or changed claim')
                        if 'value' in oracle:
                            require(block['value'] == oracle['value'], 'Wrong reviewed value')
                        for text in oracle.get('text_contains', []):
                            require(text in block['text'], 'Lost independently reviewed condition/value')
                    require(set(entity['warnings']) <= set(block['warnings']), 'Entity caveat lost')
                    if q.get('warning_contains'):
                        require(any(q['warning_contains'] in w for w in block['warnings']), 'Required freshness warning missing')
                returned_sources = {s['id']: s for s in response['sources']}
                require(set(returned_sources) == used_sources, 'Missing or unrelated response sources')
                for sid, source in returned_sources.items():
                    require(source == sources[sid] == rows[sid]['source'], 'Wrong source revision/provenance')
            except (AssertionError, KeyError, ValueError, subprocess.TimeoutExpired) as exc:
                error = str(exc)
            evidence = {'id': q['id'], 'question': q['question'], 'request': q['request'],
                        'category': q['category'], 'case': q['case'], 'passed': error is None,
                        'error': error, 'elapsed_ms': round((time.monotonic()-started)*1000, 2),
                        'response': response}
            (output / (q['id'] + '.json')).write_text(json.dumps(evidence, indent=2, ensure_ascii=False) + '\n')
            results.append({k: v for k, v in evidence.items() if k != 'response'})
    report = {'catalog_sha256': hashlib.sha256(catalog_path.read_bytes()).hexdigest(),
              'fixture_sha256': hashlib.sha256(fixture_path.read_bytes()).hexdigest(),
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'reference_now_ms': now, 'normal_category_counts': dict(normal_counts),
              'case_counts': dict(collections.Counter(q['case'] for q in questions)),
              'reviewed_fact_count': len(fixture['facts']), 'total': len(results),
              'passed': sum(r['passed'] for r in results), 'failed': sum(not r['passed'] for r in results),
              'scope': 'Offline CLI and pinned corpus only; no live freshness, Discord delivery, or source-access qualification.',
              'results': results}
    (output / 'report.json').write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
    print(json.dumps({k: v for k, v in report.items() if k != 'results'}, indent=2))
    return report['failed'] == 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', required=True, type=Path)
    parser.add_argument('--catalog', required=True, type=Path)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--fixture', type=Path, default=Path(__file__).parent / 'fixtures/release_questions.json')
    args = parser.parse_args()
    raise SystemExit(0 if run(args.corpus.resolve(), args.catalog.resolve(), args.binary.resolve(), args.fixture.resolve(), args.output.resolve()) else 1)


if __name__ == '__main__':
    main()
