"""Regression checks for the release runner's reply evidence boundary."""
import copy
import unittest
from check_release_workload import validate_reply


def card():
    return {'reply': {
        'text': 'Pebble has 2 hearts.',
        'card': {'title': 'Pebble', 'description': 'Health: 2 hearts.',
                 'fields': [{'name': 'Health', 'value': '2 hearts', 'inline': True}],
                 'footer': 'Wiki contributors — CC BY-SA 3.0'},
        'citations': [{'label': 'Pebble', 'revision': 123}],
        'buttons': [{'label': 'Ask another question', 'route': 'ask', 'options': {},
                     'prompt': {'label': 'Your question', 'option': 'question',
                                'placeholder': 'How fast is Pebble?', 'max_length': 200}}],
        'choices': [{'label': 'Pebble', 'description': 'Toon', 'route': 'lookup',
                     'options': {'name': 'Pebble', 'count': 1, 'history': False}}],
    }}


class ReplyValidationTests(unittest.TestCase):
    def test_legacy_and_card_replies(self):
        validate_reply({'reply': {'text': '2 hearts', 'citations': [{'label': 'Wiki', 'revision': 123}]}})
        validate_reply(card())

    def test_invalid_values_are_not_counted_as_successful_queries(self):
        mutations = [
            lambda r: r['reply'].update(url='https://example.org'),
            lambda r: r['reply']['card'].update(color=123),
            lambda r: r['reply']['citations'][0].update(revision=True),
            lambda r: r['reply']['citations'][0].update(revision=9007199254740992),
            lambda r: r['reply']['citations'][0].update(url='https://example.org'),
            lambda r: r['reply']['card'].update(title='😀' * 129),
            lambda r: r['reply']['card']['fields'][0].update(value='x' * 1025),
            lambda r: r['reply']['card']['fields'][0].update(inline=1),
            lambda r: r['reply']['card'].update(description='bad\u202etext'),
            lambda r: r['reply'].update(buttons=r['reply']['buttons'] * 6),
            lambda r: r['reply'].update(choices=r['reply']['choices'] * 26),
            lambda r: r['reply']['buttons'][0]['prompt'].update(max_length=201),
            lambda r: r['reply']['buttons'][0]['options'].update(question='hidden'),
            lambda r: r['reply']['choices'][0]['options'].update(count=1.5),
            lambda r: r['reply']['choices'][0]['options'].update(count=9007199254740992),
            lambda r: r['reply']['choices'][0]['options'].update(count={'nested': 1}),
            lambda r: r['reply']['choices'][0].update(route='/admin'),
            lambda r: r['reply']['choices'][0]['options'].update(one='x' * 5000, two='x' * 5000),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index):
                value = copy.deepcopy(card())
                mutate(value)
                with self.assertRaises(AssertionError):
                    validate_reply(value)

    def test_boundary_values_remain_valid(self):
        value = card()
        value['reply']['card']['title'] = '😀' * 128
        value['reply']['choices'][0]['options']['count'] = -9007199254740991
        validate_reply(value)


if __name__ == '__main__':
    unittest.main()
