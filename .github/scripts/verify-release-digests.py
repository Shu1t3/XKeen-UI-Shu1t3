"""Require direct GitHub API digests to match all built router binaries."""
import hashlib
import json
import os
from pathlib import Path
import time
from urllib.parse import quote
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def verify(paths, release, tag):
    expected = {}
    for path in paths:
        if path.name in expected:
            raise ValueError('Duplicate built asset')
        with path.open('rb') as file:
            expected[path.name] = 'sha256:' + hashlib.file_digest(file, 'sha256').hexdigest()
    if set(expected) != {'xkeen-ui-arm64-v8a', 'xkeen-ui-mips32', 'xkeen-ui-mips32le'}:
        raise ValueError('Three router binaries are required')
    if release['tag_name'] != tag or release['draft']:
        raise ValueError('Wrong or draft release')
    assets = [a for a in release['assets'] if a['name'] in expected]
    if len(assets) != 3 or len({a['name'] for a in assets}) != 3:
        raise ValueError('Missing or duplicate published asset')
    if any(a.get('digest') is None or a['state'] != 'uploaded' for a in assets):
        return False
    if {a['name']: a['digest'] for a in assets} != expected:
        raise ValueError('Published SHA-256 does not match built binary')
    return True


def main():
    repo, tag = os.environ['GITHUB_REPOSITORY'], os.environ['RELEASE_TAG']
    request = Request(f'https://api.github.com/repos/{repo}/releases/tags/{quote(tag, safe="")}', headers={'User-Agent': 'XKeen-release-integrity', 'Accept': 'application/vnd.github+json'})
    if os.environ.get('GH_TOKEN'):
        request.add_header('Authorization', 'Bearer ' + os.environ['GH_TOKEN'])
    opener = build_opener(ProxyHandler({}), NoRedirect())
    for attempt in range(6):
        with opener.open(request, timeout=60) as response:
            release = json.load(response)
        if verify(list(Path('artifacts').glob('xkeen-ui-*/*')), release, tag):
            print('All three published GitHub digests match the built binaries')
            return
        if attempt == 5:
            raise SystemExit('Published GitHub digests are missing')
        time.sleep(10)


if __name__ == '__main__':
    main()
