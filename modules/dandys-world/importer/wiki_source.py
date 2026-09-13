"""Bounded offline adapter for revision-content API snapshots; never fetches URLs."""
import hashlib
import json
from datetime import datetime
from pathlib import Path
from urllib.parse import quote

ORIGIN = 'https://dandys-world-robloxhorror.fandom.com'
MAX_PAGE = 4 * 1024 * 1024
MAX_CORPUS = 128 * 1024 * 1024


class ImportError(ValueError):
    pass


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def read(root, relative, limit):
    root = root.resolve()
    path = (root / relative).resolve()
    if not path.is_relative_to(root) or not path.is_file() or path.stat().st_size > limit:
        raise ImportError('Missing, escaping, or oversized source file')
    with path.open('rb') as handle:
        data = handle.read(limit + 1)
    if len(data) > limit:
        raise ImportError('Source grew beyond limit')
    return data.decode('utf-8')


def millis(timestamp):
    date = datetime.fromisoformat(timestamp.replace('Z', '+00:00'))
    if date.tzinfo is None:
        raise ImportError('Timezone required')
    result = int(date.timestamp() * 1000)
    if result < 0:
        raise ImportError('Invalid source timestamp')
    return result


class Corpus:
    def __init__(self, rows, manifest, namespace_counts):
        self.rows = rows
        self.manifest = manifest
        self.namespace_counts = namespace_counts
        self.images = {}
        self.by_title = {row['title']: row for row in rows}
        if len(self.by_title) != len(rows):
            raise ImportError('Duplicate source title')

    @classmethod
    def open(cls, root):
        root = Path(root)
        manifest = json.loads(read(root, 'manifest.json', MAX_PAGE))
        catalog = json.loads(read(root, 'catalog.json', 16 * 1024 * 1024))
        if manifest['status'] != 'complete' or not 1 <= len(catalog) <= 10000:
            raise ImportError('Incomplete import or page count limit')
        expected = {}
        counts = {}
        for namespace, label in manifest['namespaces'].items():
            if label not in {'articles', 'templates', 'categories', 'modules', 'maps'}:
                raise ImportError('Unexpected namespace')
            listing = json.loads(read(root, 'index-' + label + '.json', 4 * MAX_PAGE))
            counts[label] = len(listing)
            for row in listing:
                if row['pageid'] in expected or row['ns'] != int(namespace):
                    raise ImportError('Duplicate or wrong namespace in discovery')
                expected[row['pageid']] = row
        if len(expected) != len(catalog) or len(catalog) != manifest['pages'] or counts != manifest['counts']:
            raise ImportError('Coverage count mismatch')
        total, seen, rows = 0, set(), []
        for item in catalog:
            page_id = item['pageid']
            if page_id in seen or page_id not in expected:
                raise ImportError('Unexpected or repeated page')
            seen.add(page_id)
            record = json.loads(read(root, item['record_path'], MAX_PAGE * 4))
            raw = read(root, item['path'], MAX_PAGE)
            revision = record['revisions'][0]
            if raw != revision['slots']['main']['*'] or sha(raw) != item['content_sha256'] or sha(raw) != record['content_sha256']:
                raise ImportError('Corrupt source content')
            if record['title'] != item['title'] or expected[page_id]['title'] != item['title'] or record['pageid'] != page_id:
                raise ImportError('Source identity mismatch')
            if revision['revid'] != item['revision_id'] or record['ns'] != expected[page_id]['ns'] or record['ns'] != item['namespace']:
                raise ImportError('Revision/namespace mismatch')
            url = ORIGIN + '/wiki/' + quote(item['title'].replace(' ', '_'), safe='')
            if item['source_url'] != url or record['source_url'] != url:
                raise ImportError('Unapproved source URL')
            if revision['timestamp'] != item['revision_timestamp'] or record['retrieved_at'] != item['retrieved_at']:
                raise ImportError('Source timestamp mismatch')
            total += len(raw.encode())
            if total > MAX_CORPUS:
                raise ImportError('Corpus byte limit')
            rows.append({'title': item['title'], 'page_id': page_id, 'namespace': item['namespace'], 'raw': raw,
                         'source': {'id': f'page:{page_id}', 'page_id': page_id, 'title': item['title'], 'url': url,
                                    'revision_id': item['revision_id'], 'revision_timestamp': item['revision_timestamp'],
                                    'validated_at_ms': millis(item['retrieved_at']), 'content_sha256': sha(raw),
                                    'license': 'CC-BY-SA-3.0', 'license_url': 'https://creativecommons.org/licenses/by-sa/3.0/'}})
        rights = json.loads(read(root, 'siteinfo.json', MAX_PAGE))['query']['rightsinfo']
        if rights['text'] != 'CC-BY-SA':
            raise ImportError('Source license needs review')
        corpus = cls(rows, manifest, counts)
        if 'media_sha256' in manifest:
            from media_source import checksum
            media = json.loads(read(root, 'media.json', 4 * MAX_PAGE))
            if checksum(media) != manifest['media_sha256'] or media.get('version') != 1 or media.get('status') != 'complete':
                raise ImportError('Corrupt image metadata')
            images = media.get('images')
            if not isinstance(images, dict) or len(images) > len(rows):
                raise ImportError('Invalid image mapping')
            sources = {row['source']['id']: row for row in rows}
            for key, image in images.items():
                row = sources.get(key)
                if not row or row['namespace'] != 0 or not isinstance(image, dict) or image.get('article_revision') != row['source']['revision_id']:
                    raise ImportError('Image article revision mismatch')
            corpus.images = images
        return corpus

    def citation(self, row, section, raw):
        if raw not in row['raw']:
            raise ImportError('Citation fragment absent from source')
        return {'source_id': row['source']['id'], 'section': section, 'quote': raw}
