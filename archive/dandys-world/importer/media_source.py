"""Optional main-image metadata from the canonical wiki; never downloads images."""
import hashlib
import json
import re
from urllib.parse import unquote, urlsplit

from wiki_source import ImportError, millis

PREFIX = 'https://static.wikia.nocookie.net/dandys-world-robloxhorror/images/'
MIMES = {'image/png', 'image/jpeg', 'image/gif', 'image/webp'}


def image_url(value):
    if not isinstance(value, str) or len(value) > 2048 or not value.startswith(PREFIX):
        return False
    parsed = urlsplit(value)
    decoded = unquote(parsed.path)
    return (not parsed.fragment and not parsed.username and not parsed.password
            and not any(ord(c) < 32 or ord(c) == 127 or c == '\\' for c in decoded)
            and re.fullmatch(r'/dandys-world-robloxhorror/images/[0-9a-f]/[0-9a-f]{2}/[^/]+\.(?:png|jpg|jpeg|gif|webp)/revision/latest(?:/scale-to-width-down/256)?', decoded, re.I) is not None
            and re.fullmatch(r'cb=[0-9]{1,20}', parsed.query) is not None
            and all(part not in {'.', '..'} for part in decoded.split('/')))


def positive(value):
    return type(value) is int and 0 < value <= 9_007_199_254_740_991


def pages(value):
    result = value.get('query', {}).get('pages')
    if not isinstance(result, list) or len(result) > 50:
        raise ImportError('Malformed image metadata response')
    return result


def first(value):
    return value[0] if isinstance(value, list) and value and isinstance(value[0], dict) else {}


def collect(corpus, client):
    """Match pageimages to exact saved article revisions, then inspect file metadata.

    No shared-source child entity inherits an article's image; Normalizer further
    intersects this mapping with exact own-page entity identities.
    """
    own = [row for row in corpus.rows if row['namespace'] == 0
           and not row['raw'].lstrip().lower().startswith('#redirect')]
    selected = {}
    for offset in range(0, len(own), 50):
        batch = own[offset:offset + 50]
        expected = {row['page_id']: row for row in batch}
        value, checked = client.get(prop='pageimages|revisions', pageids='|'.join(map(str, expected)),
                                    piprop='original|name', rvprop='ids')
        seen = set()
        for page in pages(value):
            if not isinstance(page, dict):
                raise ImportError('Malformed image article')
            pid = page.get('pageid')
            if pid not in expected or pid in seen:
                raise ImportError('Unexpected image article identity')
            seen.add(pid)
            row = expected[pid]
            if (page.get('title') != row['title'] or page.get('ns') != 0
                    or first(page.get('revisions')).get('revid') != row['source']['revision_id']):
                continue  # Saved article differs: never attach a new revision's image.
            filename = page.get('pageimage')
            original = page.get('original', {})
            if not filename or not isinstance(original, dict) or not image_url(original.get('source')):
                continue
            if not isinstance(filename, str) or len(filename) > 240 or any(c in filename for c in '|\n\r'):
                raise ImportError('Malformed image filename')
            selected[pid] = {'row': row, 'filename': filename, 'original': original['source'], 'checked': checked}
        if seen != set(expected):
            raise ImportError('Missing image article metadata')
    titles = sorted({'File:' + x['filename'].replace('_', ' ') for x in selected.values()})
    files = {}
    for offset in range(0, len(titles), 50):
        batch = titles[offset:offset + 50]
        value, checked = client.get(prop='imageinfo|revisions', titles='|'.join(batch), rvprop='ids',
                                    iiprop='url|size|mime|sha1|timestamp|extmetadata', iiurlwidth=256)
        for page in pages(value):
            if not isinstance(page, dict) or not isinstance(page.get('title'), str):
                raise ImportError('Malformed image file')
            title = page.get('title', '').replace('_', ' ')
            if title not in batch or title in files:
                raise ImportError('Unexpected image file identity')
            files[title] = (page, checked)
    images, evidence = {}, {}
    for pid, choice in selected.items():
        title = 'File:' + choice['filename'].replace('_', ' ')
        if title not in files:
            continue
        page, checked = files[title]
        info = first(page.get('imageinfo'))
        revision = first(page.get('revisions')).get('revid')
        url = info.get('thumburl', info.get('url'))
        width = info.get('thumbwidth', info.get('width'))
        height = info.get('thumbheight', info.get('height'))
        digest = info.get('sha1', '')
        if not isinstance(digest, str):
            continue
        if not re.fullmatch('[0-9a-f]{40}', digest):
            try:
                digest = f'{int(digest, 36):040x}' if re.fullmatch('[0-9a-z]{1,31}', digest) else ''
            except ValueError:
                digest = ''
        if (page.get('ns') != 6 or not positive(page.get('pageid')) or not positive(revision)
                or info.get('mime') not in MIMES or not image_url(url)
                or info.get('url') != choice['original']
                or not positive(width) or not positive(height) or max(width, height) > 16384
                or not positive(info.get('size')) or info['size'] > 20 * 1024 * 1024
                or not re.fullmatch('[0-9a-f]{40}', digest)):
            continue
        images[f'page:{pid}'] = dict(url=url, file_title=page['title'], file_page_id=page['pageid'],
                                   revision=revision, sha1=digest, mime=info['mime'], width=width, height=height,
                                   validated_at_ms=min(millis(checked), millis(choice['checked'])),
                                   article_revision=choice['row']['source']['revision_id'])
        evidence[page['title']] = {'descriptionurl': info.get('descriptionurl'),
                                  'upload_timestamp': info.get('timestamp'),
                                  'metadata': info.get('extmetadata', {}),
                                  'license_note': 'Article text license does not establish image rights.'}
    return {'version': 1, 'status': 'complete', 'images': images, 'file_evidence': evidence}


def encoded(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()


def checksum(value):
    return hashlib.sha256(encoded(value)).hexdigest()
