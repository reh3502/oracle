"""Image acquisition checks through the source importer; no live requests."""
import json
import tempfile
import unittest
from pathlib import Path

from media_source import PREFIX, collect
from refresh_source import acquire
from test_refresh_source import Wiki
from wiki_source import Corpus, ImportError

URL = PREFIX + '8/8d/Pebble_Render.png/revision/latest?cb=20240806022953'
THUMB = URL.replace('/latest?', '/latest/scale-to-width-down/256?')


class MediaTests(unittest.TestCase):
    def wiki(self, changed=False, missing=False):
        wiki = Wiki()
        def media(_, q):
            if q.get('prop') == 'pageimages|revisions':
                pages = [{'pageid': 1, 'ns': 0, 'title': 'Page', 'revisions': [{'revid': 11 if changed else 10}],
                          'pageimage': 'Pebble_Render.png', 'original': {'source': URL}}]
            elif q.get('prop') == 'imageinfo|revisions':
                pages = [{'pageid': 100, 'ns': 6, 'title': 'File:Pebble Render.png', 'revisions': [{'revid': 200}],
                          'imageinfo': [] if missing else [{'url': URL, 'thumburl': THUMB, 'thumbwidth': 256,
                              'thumbheight': 256, 'width': 700, 'height': 700, 'size': 223790,
                              'mime': 'image/png', 'sha1': 'd339e479230b9ac09914124f974d1f69b09afa6f'}]}]
            else:
                return None
            return 200, {'Content-Type': 'application/json'}, json.dumps({'query': {'pages': pages}}).encode()
        wiki.change = media
        return wiki

    def test_acquisition_preserves_verified_thumbnail_and_detects_tampering(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'source'
            acquire(path, client=self.wiki().client(), include_images=True)
            saved = Corpus.open(path)
            image = saved.images['page:1']
            self.assertEqual(image['url'], THUMB)
            self.assertEqual(image['revision'], 200)
            self.assertEqual(image['article_revision'], 10)
            media = json.loads((path / 'media.json').read_text())
            media['images']['page:1']['url'] = 'https://example.com/other.png'
            (path / 'media.json').write_text(json.dumps(media))
            with self.assertRaisesRegex(ImportError, 'Corrupt image'):
                Corpus.open(path)

    def test_changed_article_or_missing_file_keeps_text_only(self):
        for options in ({'changed': True}, {'missing': True}):
            with self.subTest(options=options), tempfile.TemporaryDirectory() as root:
                path = Path(root) / 'source'
                acquire(path, client=self.wiki(**options).client(), include_images=True)
                self.assertEqual(Corpus.open(path).images, {})


if __name__ == '__main__':
    unittest.main()
