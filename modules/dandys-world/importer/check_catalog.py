"""Verify a complete offline import and exercise the Rust CLI against it.

Run with the importer virtualenv; corpus/catalog/store artifacts stay ignored.
Expected game values below qualify the reviewed September 2026 source snapshot.
They are acceptance assertions, never the runtime's entity roster or fact source.
"""
import argparse
import collections
import json
import subprocess
import tempfile
import time
from pathlib import Path

from wiki_source import Corpus
from normalize import redirect, templates
from wikitext import normalize_name


def check(corpus_path, catalog_path, binary):
    corpus = Corpus.open(corpus_path)
    data = json.loads(catalog_path.read_text())
    sources = {r['source']['id']: r for r in corpus.rows}
    assert data['sources'] == [r['source'] for r in corpus.rows]
    assert data['coverage']['discovered_pages'] == len(corpus.rows)
    assert data['coverage']['imported_pages'] == len(corpus.rows)
    identities = {e['id']: e for e in data['entities']}
    excluded = {e['page_id'] for e in data['coverage']['excluded']}
    roster = collections.Counter()
    for row in corpus.rows:
        if row['namespace'] != 0 or redirect(row['raw']):
            continue
        identity = 'page:' + str(row['page_id'])
        assert identity in identities or row['page_id'] in excluded, row['title']
        boxes = {normalize_name(t.name) for t in templates(row['raw'])}
        for template, kind in [('Toons', 'toon'), ('Twisted', 'twisted'), ('Trinket', 'trinket')]:
            if template in boxes and row['title'] != 'Minor Characters':
                expected = 'npc' if row['title'] in {'Dandy', 'Dyle'} else kind
                assert identities[identity]['kind'] == expected, row['title']
                assert identities[identity]['facts'], row['title']
                roster[expected] += 1
    citation_count = 0
    for entity in data['entities']:
        assert entity['facts'], entity['name']
        for record in entity['facts'] + entity['relationships']:
            assert record['citations'], (entity['name'], record)
            for citation in record['citations']:
                assert citation['quote'] in sources[citation['source_id']]['raw']
                citation_count += 1
    assert roster == {'toon': 40, 'twisted': 42, 'trinket': 58, 'npc': 2}, roster
    kinds = collections.Counter(e['kind'] for e in data['entities'])
    assert all(kinds[k] for k in ['toon', 'twisted', 'npc', 'floor', 'machine', 'mechanic', 'trinket', 'item', 'event'])
    assert (kinds['floor'], kinds['item'], kinds['machine']) == (21, 30, 5)
    now = max(s['validated_at_ms'] for s in data['sources']) + 1000
    query_times = []
    checked = []
    with tempfile.TemporaryDirectory(prefix='dw-qualification-') as temp:
        store = Path(temp) / 'store'
        def cli(*args, good=True):
            result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True)
            assert (result.returncode == 0) == good, result.stderr
            return json.loads(result.stdout) if good else None
        publication = cli('publish', '--catalog', catalog_path, '--store', store)
        def query(request, at=now):
            start = time.perf_counter()
            result = cli('query', '--store', store, '--input', json.dumps(request), '--now-ms', at)
            query_times.append((time.perf_counter()-start)*1000)
            assert result['snapshot_id'] == publication['snapshot_id']
            for block in result['answer_blocks']:
                assert block['citations'] and block['source_ids']
                assert set(block['source_ids']) <= {s['id'] for s in result['sources']}
            checked.append(request)
            return result
        def lookup(name, kind, field):
            return query({'op':'lookup','name':name,'kind':kind,'field':field})['answer_blocks']
        def block(blocks, key):
            return next(b for b in blocks if b['key'] == key)
        assert query({'op':'lookup','name':'Pebble'})['status'] == 'needs_clarification'
        result = query({'op':'ask','question':'How do I unlock Pebble?'})
        assert len(block(result['answer_blocks'], 'requirements')['value']['all']) == 3
        assert block(lookup('Pebble','toon','health'),'health')['value'] == 2
        movement = block(lookup('Pebble','toon','movement speed'),'movement_speed')
        assert movement['value']['walk'] == 20 and movement['value']['sprint'] == 30
        ability = query({'op':'ask','question':'What does Toon Pebble do?'})
        assert any('-40' in b['text'] and '60' in b['text'] for b in ability['answer_blocks'])
        speed = block(lookup('Twisted Pebble','twisted','speed'),'speed')
        assert all(t in speed['text'] for t in ['24 chasing','28.8 chasing','27.6 chasing'])
        bone = block(lookup('Bone','trinket','effect'),'effect')
        assert all(t in bone['text'] for t in ['25%', '4 seconds', '40', 'Capsules and Tapes'])
        requirements = block(lookup('Boxten','toon','requirements'),'requirements')
        assert any('any' in part for part in requirements['value']['all'])
        razzle = block(lookup('Razzle & Dazzle','toon','movement speed'),'movement_speed')
        assert [v['stars'] for v in razzle['value']['variants']] == [5,1]
        assert razzle['conditions']
        for name in ['Bandage', 'Health Kit']:
            prices = lookup(name,'item','price')
            both = next(b for b in prices if 'both discounts' in b['conditions'])
            assert both['state'] == 'conflicting' and both['value'] is None
        boxten = block(lookup('Boxten','toon','ability'),'ability_1')
        assert boxten['state'] == 'conflicting' and boxten['value'] is None
        research = query({'op':'ask','question':'How does research work?'})
        assert research['candidates'][0]['kind'] == 'mechanic'
        assert research['answer_blocks']
        assert lookup('Blackouts','mechanic','effect')
        blackout = query({'op':'ask','question':'What happens during a blackout?'})
        assert blackout['status'] == 'answered'
        assert any('darkness' in b['text'] for b in blackout['answer_blocks'])
        assert '3 variants' in block(lookup('Warehouse','floor','variants'),'variants')['text']
        assert 'Floor 6+' in block(lookup('Warehouse','floor','requirements'),'requirements')['text']
        assert '7 variants' in block(lookup('Rainbow Rooms','floor','variants'),'variants')['text']
        for name in ['Easter Floor', 'Seasonal Room - Easter']:
            requirements = block(lookup(name,'floor','requirements'),'requirements')['text']
            assert all(x in requirements for x in ['every 5th', '50%', 'Easter Event-exclusive'])
        assert block(lookup("Pebble's Floor",'floor','requirements'),'requirements')['state'] == 'historical'
        assert '25th' in block(lookup('Break Room','floor','requirements'),'requirements')['text']
        assert query({'op':'lookup','name':'Treadmill Machine','kind':'machine'})['answer_blocks']
        assert block(lookup('Soulvester','toon','health'),'health')['value'] == 4
        assert block(lookup('Shrimpo','toon','stealth'),'stealth')['value']['priority'] == -99
        for kind in ['toon','twisted','npc','floor','machine','mechanic','trinket','item','event']:
            entity = next(e for e in data['entities'] if e['kind']==kind and e['facts'])
            result = query({'op':'lookup','name':entity['id']})
            assert result['answer_blocks'], (kind, entity['name'])
        comparison = query({'op':'compare','left':'page:193','right':next(e['id'] for e in data['entities'] if e['name']=='Poppy' and e['kind']=='toon'),'field':'movement speed'})
        assert comparison['status'] == 'unsupported_query'  # Poppy includes an ability condition.
        comparison = query({'op':'compare','left':'page:193','right':next(e['id'] for e in data['entities'] if e['name']=='Astro' and e['kind']=='toon'),'field':'stamina'})
        assert len(comparison['answer_blocks']) == 2
        assert query({'op':'lookup','name':'Pebbble'})['status'] != 'answered'
        stale = query({'op':'lookup','name':'Pebble','kind':'toon','field':'health'}, now+8*86400000)
        assert all(b['value'] is None for b in stale['answer_blocks'])
        # A malformed candidate cannot replace the active complete catalog.
        broken = Path(temp)/'broken.json'
        broken.write_text('{"schema_version":999}')
        cli('publish','--catalog',broken,'--store',store,good=False)
        assert query({'op':'status'})['snapshot_id'] == publication['snapshot_id']
    return {'snapshot_id':publication['snapshot_id'],'sources':len(sources),'entities':len(identities),
            'roster':dict(roster),'entities_by_kind':dict(kinds),'verified_citations':citation_count,
            'fact_states':dict(collections.Counter(f['state'] for e in data['entities'] for f in e['facts'])),
            'queries':len(checked),'cli_cold_query_ms':{'min':min(query_times),'max':max(query_times),'mean':sum(query_times)/len(query_times)},
            'clock':'latest source validation + 1 second; stale case + 8 days','passed':True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus',type=Path,required=True)
    parser.add_argument('--catalog',type=Path,required=True)
    parser.add_argument('--binary',type=Path,required=True)
    args = parser.parse_args()
    print(json.dumps(check(args.corpus,args.catalog,args.binary.resolve()),indent=2))
