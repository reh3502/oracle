"""Offline importer contracts; real facts use attributed revision-pinned excerpts.

Synthetic pages below exercise wiki syntax only and are not game-data fixtures.
"""
import json
import tempfile
import unittest
from pathlib import Path
from urllib.parse import quote

import mwparserfromhell as mw
from normalize import Normalizer, table_rows, templates
from wiki_source import Corpus, ImportError, ORIGIN, millis, read, sha
from wikitext import Renderer, Unsupported, expression

FIXTURES = Path(__file__).resolve().parents[3] / 'prototypes/dw-source/fixtures/wiki.json'


def corpus(extra=(), include_fixtures=True):
    rows = []
    for item in json.loads(FIXTURES.read_text()) if include_fixtures else []:
        raw = item['wikitext']
        assert sha(raw) == item['excerpt_sha256'], 'Attributed fixture changed'
        source = {'id': f"page:{item['pageid']}", 'page_id': item['pageid'],
                  'title': item['title'], 'url': item['source_url'],
                  'revision_id': item['revision_id'], 'revision_timestamp': item['revision_timestamp'],
                  'validated_at_ms': millis(item['retrieved_at']), 'content_sha256': sha(raw),
                  'license': item['license'], 'license_url': item['license_url']}
        rows.append({'title': item['title'], 'page_id': item['pageid'],
                     'namespace': 10 if item['title'].startswith('Template:') else 0,
                     'raw': raw, 'source': source})
    for index, (title, raw) in enumerate(extra, 900000):
        source = {'id': f'page:{index}', 'page_id': index, 'title': title,
                  'url': ORIGIN + '/wiki/' + quote(title.replace(' ', '_'), safe=''),
                  'revision_id': index, 'revision_timestamp': '2026-01-01T00:00:00Z',
                  'validated_at_ms': millis('2026-01-01T00:00:00Z'), 'content_sha256': sha(raw),
                  'license': 'CC0-1.0', 'license_url': 'https://creativecommons.org/publicdomain/zero/1.0/'}
        rows.append({'title': title, 'page_id': index,
                     'namespace': 10 if title.startswith('Template:') else 0, 'raw': raw, 'source': source})
    return Corpus(rows, {'started_at': '2026-01-01T00:00:00Z',
                         'completed_at': '2026-01-01T00:00:01Z'}, {})


def infobox(c, title, kind='toon'):
    n = Normalizer(c)
    row = c.by_title[title]
    entity = n.entity(row, kind)
    n.infobox(entity, row, templates(row['raw'])[0])
    return {f['key']: f for f in entity['facts']}


class ArithmeticTests(unittest.TestCase):
    def test_reviewed_arithmetic_and_mediawiki_rounding(self):
        self.assertEqual(expression('(5*2.5)+7.5'), 20)
        self.assertEqual(expression('1.25 round 1'), 1.3)
        self.assertEqual(expression('-1.25 round 1'), -1.3)
        self.assertEqual(expression('5 mod 2 = 1'), 1)

    def test_expressions_cannot_execute_python(self):
        for raw in ["__import__('os').system('false')", '(1).__class__', '[n for n in (1,2)]',
                    'open("synthetic-marker", "w")', 'lambda: 1']:
            with self.subTest(raw=raw), self.assertRaises(Unsupported):
                expression(raw)

    def test_arithmetic_limits_fail_closed(self):
        for raw in ['2^9999', '1/0', '1e309', '9 round 100']:
            with self.subTest(raw=raw), self.assertRaises(Unsupported):
                expression(raw)


class RenderingTests(unittest.TestCase):
    def test_switch_defaults_and_fallthrough_follow_wiki_semantics(self):
        c = corpus([('Syntax', ''), ('Template:Count', '{{#switch:{{{type}}}|A=1|42}}')], False)
        for raw, expected in [('{{Count}}', '42'),
                              ('{{#switch:X|A=1|#default=9}}', '9'),
                              ('{{#switch:A|A|B=2|0}}', '2')]:
            r = Renderer(c, c.by_title['Syntax'])
            self.assertEqual(r.text(raw), expected)
            self.assertFalse(r.unresolved)

    def test_unexpanded_arguments_are_not_supported_answers(self):
        c = corpus([('Syntax', '{{{missing}}}')], False)
        r = Renderer(c, c.by_title['Syntax'])
        r.text(c.by_title['Syntax']['raw'])
        self.assertTrue(r.unresolved)

    def test_nested_ability_keeps_both_template_revisions(self):
        c = corpus()
        facts = infobox(c, 'Pebble')
        ability = facts['ability_1']
        self.assertEqual(ability['state'], 'supported')
        self.assertIn('Speak!', ability['text'])
        self.assertIn('-40', ability['text'])
        self.assertIn('cooldown of 60', ability['text'])
        cited = {x['source_id'] for x in ability['citations']}
        for title in ['Pebble', 'Template:AI', 'Template:AbilityIcon']:
            self.assertIn(c.by_title[title]['source']['id'], cited)
        for citation in ability['citations']:
            row = next(r for r in c.rows if r['source']['id'] == citation['source_id'])
            self.assertIn(citation['quote'], row['raw'])

    def test_unknown_condition_never_selects_a_supported_branch(self):
        c = corpus([('Syntax', '{{#if:{{UnknownSyntax}}|yes|no}}')], False)
        r = Renderer(c, c.by_title['Syntax'])
        self.assertNotIn(r.text(c.by_title['Syntax']['raw']), ('yes', 'no'))
        self.assertTrue(r.unresolved)

    def test_template_cycle_is_bounded_and_unresolved(self):
        c = corpus([('Syntax', '{{Cycle}}'), ('Template:Cycle', '{{Cycle}}')], False)
        r = Renderer(c, c.by_title['Syntax'])
        r.text('{{Cycle}}')
        self.assertTrue(r.unresolved)
        self.assertLess(r.steps, 12001)

    def test_unsafe_tag_is_not_answerable(self):
        c = corpus([('Syntax', '<script>alert(1)</script>')], False)
        r = Renderer(c, c.by_title['Syntax'])
        self.assertNotIn('alert', r.text(c.by_title['Syntax']['raw']))
        self.assertTrue(r.unresolved)

    def test_citation_cannot_quote_another_page(self):
        c = corpus()
        with self.assertRaises(ImportError):
            c.citation(c.by_title['Pebble'], 'Synthetic', 'not present in this source')


class InfoboxTests(unittest.TestCase):
    def test_explicit_negative_stealth_is_not_inferred_from_stars(self):
        c = corpus([('Syntax', '{{Toons|stealth={{Star}}<br>{{Small|(-99)}}}}')], False)
        f = infobox(c, 'Syntax')['stealth']
        self.assertEqual(f['value'], {'stars':1,'priority':-99})
        self.assertEqual(f['state'], 'supported')

    def test_floor_event_infobox_retains_effect_and_chance(self):
        c = corpus([('Syntax', '{{EffectofEvent|effect=Visibility reduced|chance=Only after floor 3}}')], False)
        f = infobox(c, 'Syntax', 'mechanic')
        self.assertEqual(f['effect']['text'], 'Visibility reduced')
        self.assertEqual(f['chance']['text'], 'Only after floor 3')

    def test_disputed_field_requires_review_after_revision_change(self):
        c = corpus([('Boxten', '{{Toons|ability_1=Changed ability}}')], False)
        n = Normalizer(c)
        row = c.by_title['Boxten']
        entity = n.entity(row, 'toon')
        n.infobox(entity, row, templates(row['raw'])[0])
        n.apply_reviews()
        self.assertEqual(entity['facts'][0]['state'], 'unverified')
        self.assertIsNone(entity['facts'][0]['value'])
        self.assertTrue(any('new source revisions' in w for w in entity['warnings']))

    def test_attributed_base_stats_have_units_and_template_evidence(self):
        c = corpus()
        f = infobox(c, 'Pebble')
        self.assertEqual(f['movement_speed']['value'], {'stars': 5, 'walk': 20, 'sprint': 30})
        self.assertEqual(f['stamina']['value'], {'stars': 4, 'capacity': 175})
        self.assertEqual(f['extraction_speed']['value'], {'stars': 1, 'rate': .75})
        self.assertIn(c.by_title['Template:StatComp']['source']['id'],
                      {x['source_id'] for x in f['movement_speed']['citations']})

    def test_main_heart_is_not_a_third_health_point(self):
        self.assertEqual(infobox(corpus(), 'Pebble')['health']['value'], 2)
        self.assertEqual(infobox(corpus(), 'Dandy', 'npc')['health']['value'], 3)

    def test_unknown_heart_icon_cannot_produce_numeric_health(self):
        c = corpus([('Syntax', '{{Toons|heart1=Unreviewed Heart|heart2=Heart}}')], False)
        self.assertNotIn('health', infobox(c, 'Syntax'))

    def test_conditional_stars_keep_both_variants_and_context(self):
        # Synthetic syntax based on the reviewed odd/even layout, not a gameplay claim.
        raw = "{{Toons|movement_speed='''Odd Floors''': {{StatComp|Move|5}}<br>'''Even Floors''': {{StatComp|Move|1}}}}"
        f = infobox(corpus([('Syntax', raw)]), 'Syntax')['movement_speed']
        self.assertEqual([v['stars'] for v in f['value']['variants']], [5, 1])
        self.assertIn('Odd Floors', f['text'])
        self.assertIn('Even Floors', f['text'])
        self.assertTrue(any('unconditional' in x for x in f['conditions']))
        self.assertTrue(any('Odd Floors' in x for x in f['conditions']))

    def test_different_conditional_stats_cannot_share_comparison_conditions(self):
        a = '{{Toons|movement_speed={{StatComp|Move|3}}<br>At one heart: 21}}'
        b = '{{Toons|movement_speed={{StatComp|Move|3}}<br>During ability: 22.5}}'
        c = corpus([('One', a), ('Two', b)])
        self.assertNotEqual(infobox(c, 'One')['movement_speed']['conditions'],
                            infobox(c, 'Two')['movement_speed']['conditions'])

    def test_source_quality_warning_prevents_confident_table_answer(self):
        c = corpus([('Syntax', 'This table is outdated. Values: 1, 2, 3.')], False)
        n = Normalizer(c)
        row = c.by_title['Syntax']
        entity = n.entity(row, 'mechanic')
        n.sections(entity, row)
        self.assertEqual(entity['facts'][0]['state'], 'unverified')
        self.assertIsNone(entity['facts'][0]['value'])

    def test_nested_strategy_sections_inherit_uncertainty_until_sibling(self):
        raw = ('== Strategy ==\nAdvice.\n=== Survivability ===\nA ranking claim.\n'
               '==== Details ====\nMore advice.\n== Mechanics ==\nA documented mechanic.\n'
               '=== Timing ===\nA documented duration.')
        c = corpus([('Syntax', raw)], False)
        n = Normalizer(c)
        row = c.by_title['Syntax']
        entity = n.entity(row, 'mechanic')
        n.sections(entity, row)
        facts = {f['key']: f for f in entity['facts']}
        for name in ('strategy', 'survivability', 'details'):
            self.assertEqual(facts[name]['state'], 'unverified')
            self.assertIsNone(facts[name]['value'])
        for name in ('mechanics', 'timing'):
            self.assertEqual(facts[name]['state'], 'supported')

    def test_stat_template_drift_does_not_reuse_formula(self):
        c = corpus()
        c.by_title['Template:StatComp']['raw'] += '\nchanged'
        f = infobox(c, 'Pebble')['movement_speed']
        self.assertEqual(f['state'], 'unverified')
        self.assertIsNone(f['value'])

    def test_unlock_conjunction_preserves_all_three_requirements(self):
        f = infobox(corpus(), 'Pebble')['requirements']
        self.assertEqual(len(f['value']['all']), 3)
        self.assertIn('3750 Ichor', f['text'])
        self.assertIn('100% Research', f['text'])
        self.assertIn('all Mastery Quests', f['text'])

    def test_unlock_alternative_is_nested_inside_conjunction(self):
        raw = '{{Toons|requirement_1=Free<br>or<br>250 tokens|requirement_2=Complete quest}}'
        f = infobox(corpus([('Syntax', raw)], False), 'Syntax')['requirements']
        self.assertEqual(f['value']['all'], [{'any': [{'text': 'Free'}, {'text': '250 tokens'}]},
                                            {'text': 'Complete quest'}])

    def test_missing_value_is_unknown_not_zero(self):
        c = corpus([('Syntax', '{{Trinket|effect=}}')], False)
        f = infobox(c, 'Syntax', 'trinket')['effect']
        self.assertEqual(f['state'], 'unknown')
        self.assertIsNone(f['value'])

    def test_duplicate_field_cannot_pick_last_value_as_fact(self):
        c = corpus([('Syntax', '{{Trinket|effect=First|effect=Second}}')], False)
        f = infobox(c, 'Syntax', 'trinket')['effect']
        self.assertEqual(f['state'], 'conflicting')
        self.assertIsNone(f['value'])


class TableAndAliasTests(unittest.TestCase):
    def test_spans_inherit_original_cell_text_and_header(self):
        raw = '{|\n! Name !! Rarity !! Effect\n|-\n| A\n| rowspan="2" | Rare\n| rowspan="2" | Shared effect\n|-\n| B\n|-\n| colspan="3" | Unavailable\n|}'
        rows = table_rows(mw.parse(raw).filter_tags()[0])
        self.assertTrue(all(c['header'] for c in rows[0]))
        self.assertEqual([c['raw'].strip() for c in rows[2]], ['B', 'Rare', 'Shared effect'])
        self.assertEqual([c['raw'].strip() for c in rows[3]], ['Unavailable'] * 3)

    def test_adjacent_attributes_preserve_rowspan(self):
        raw = '{|\n| A\n| style="text-align:center;"rowspan="2" | Shared\n|-\n| B\n|}'
        rows = table_rows(mw.parse(raw).filter_tags()[0])
        self.assertEqual([c['raw'].strip() for c in rows[1]], ['B', 'Shared'])

    def test_commented_rowspan_does_not_create_inherited_data(self):
        raw = '{|\n| A\n| style="text-align:center;" <!--rowspan="2"--> | First\n|-\n| B || Second\n|}'
        rows = table_rows(mw.parse(raw).filter_tags()[0])
        self.assertEqual([c['raw'].strip() for c in rows[1]], ['B', 'Second'])

    def test_blank_table_cell_is_preserved_not_numeric_zero(self):
        raw = '{|\n| A || || C\n|}'
        rows = table_rows(mw.parse(raw).filter_tags()[0])
        self.assertEqual(rows[0][1]['raw'].strip(), '')

    def test_oversized_table_spans_are_rejected(self):
        raw = '{|\n| colspan="1000000" | X\n|}'
        with self.assertRaises(ImportError):
            table_rows(mw.parse(raw).filter_tags()[0])

    def test_redirect_cycles_and_missing_targets_are_reported(self):
        c = corpus([('A', '#REDIRECT [[B]]'), ('B', '#REDIRECT [[A]]'),
                    ('Lost', '#REDIRECT [[Absent]]')], False)
        n = Normalizer(c)
        n.aliases_and_relationships()
        self.assertEqual({r['title'] for r in n.unresolved}, {'A', 'B', 'Lost'})
        self.assertFalse(n.entities)

    def test_redirect_chain_retains_anchor_entity(self):
        c = corpus([('Alias', '#REDIRECT [[Bridge]]'), ('Bridge', '#REDIRECT [[Container#Named]]'),
                    ('Container', 'Named')], False)
        n = Normalizer(c)
        e = n.entity(c.by_title['Container'], 'item', 'Named')
        n.aliases_and_relationships()
        self.assertEqual(e['aliases'], ['Alias', 'Bridge'])
        self.assertFalse(n.unresolved)

    def test_template_and_npc_classification_is_specific(self):
        c = corpus()
        n = Normalizer(c)
        for title, kind in [('Pebble', 'toon'), ('Twisted Pebble', 'twisted'),
                            ('Dandy', 'npc'), ('Bone', 'trinket'), ('Events', 'event')]:
            with self.subTest(title=title):
                self.assertEqual(n.classify(c.by_title[title]), kind)

    def test_complete_synthetic_pipeline_covers_all_categories_and_attribution(self):
        pages = [
            ('Art Gallery', 'A named playable floor. [[Category:Floors]]'),
            ('Example Toon', '{{Toons|heart1=Heart|ability_1=Example effect}}'),
            ('Twisted Example Toon', '{{Twisted|mechanic=Example Toon counterpart}}'),
            ('Example Trinket', '{{Trinket|effect=Example effect}}'),
            ('Example Person', 'Example person. [[Category:Humans]]'),
            ('Example Event', '{{Infobox_Event|start_date=Unknown}}'),
            ('Health', 'Example mechanic.'),
            ('Machines', '==Mechanics==\n===Default Machine===\nExample machine.'),
            ('Floors', '==List of Floors==\n{|\n! colspan="2" | Name !! Image !! Variants !! Requirements\n|-\n| colspan="2" | [[Example Floor]]\n| Image || One || Example restriction\n|}'),
            ('Items', '==List of Items==\n{|\n! Item !! Rarity !! Effect !! Category !! Shop !! Normal !! Plush !! Frugal !! Both\n|-\n| {{II|Example Item}} || Rare || Example effect || Example category || No || 10 || 5 || 9 || 4\n|}'),
            ('Example Item', '#REDIRECT [[Items#Example Item]]'),
        ]
        c = corpus(pages, False)
        result = Normalizer(c).build()
        self.assertEqual(set(result['coverage']['entities_by_kind']),
                         {'toon', 'twisted', 'trinket', 'npc', 'event', 'mechanic', 'floor', 'item', 'machine'})
        item = next(e for e in result['entities'] if e['name'] == 'Example Item')
        self.assertEqual(item['kind'], 'item')
        prices = {f['conditions'][0]: f['value'] for f in item['facts'] if f['key'] == 'price'}
        self.assertEqual(prices, {'normal': 10, 'Dandy Plush': 5, 'Frugal Card': 9, 'both discounts': 4})
        self.assertFalse(result['coverage']['unresolved_redirects'])
        gallery = next(e for e in result['entities'] if e['name'] == 'Art Gallery')
        self.assertEqual(gallery['kind'], 'floor')
        self.assertTrue(any('playable floor' in f['text'] for f in gallery['facts']))
        sources = {r['source']['id']: r for r in c.rows}
        for entity in result['entities']:
            for fact in entity['facts']:
                self.assertTrue(fact['citations'])
                for citation in fact['citations']:
                    self.assertIn(citation['quote'], sources[citation['source_id']]['raw'])

    def test_source_paths_cannot_escape_import_root(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / 'root'
            root.mkdir()
            (Path(d) / 'outside').write_text('private')
            with self.assertRaises(ImportError):
                read(root, '../outside', 100)


if __name__ == '__main__':
    unittest.main()
