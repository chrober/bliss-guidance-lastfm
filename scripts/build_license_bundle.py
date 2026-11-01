#!/usr/bin/env python3
"""Collect vendored license and notice material from a source snapshot."""
import json
from pathlib import Path, PurePosixPath
import sys
import tarfile
import zipfile


def build(source, output):
    if output.exists():
        raise FileExistsError(f'refusing to overwrite {output}')
    with tarfile.open(source, 'r:gz') as archive:
        selected = {}
        for member in archive:
            path = PurePosixPath(member.name)
            if path.is_absolute() or '..' in path.parts or '\\' in member.name or ':' in member.name:
                raise ValueError(f'unsafe archive path: {member.name}')
            if not member.isfile() or len(path.parts) < 2 or path.parts[0] != 'source':
                continue
            relative = PurePosixPath(*path.parts[1:])
            is_manifest = str(relative) == 'SOURCE_MANIFEST.json'
            is_vendor_notice = (relative.parts[0] == 'vendor' and
                                path.name.upper().startswith(('LICENSE', 'COPYING', 'NOTICE', 'COPYRIGHT')))
            is_supplement = relative.parts[0] == 'third-party-licenses'
            if is_manifest or is_vendor_notice or is_supplement:
                key = relative.as_posix()
                if key in selected:
                    raise ValueError(f'duplicate archive member: {key}')
                selected[key] = archive.extractfile(member).read()
        manifest = json.loads(selected['SOURCE_MANIFEST.json'])
        if manifest.get('schema_version') != 1 or not isinstance(manifest.get('packages'), list):
            raise ValueError('invalid dependency inventory')
    output.parent.mkdir(parents=True, exist_ok=True)
    created = False
    try:
        with output.open('xb') as stream:
            created = True
            with zipfile.ZipFile(stream, 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
                bundle.writestr('README.txt',
                    'License and notice files collected from the vendored source snapshot.\n'
                    'SOURCE_MANIFEST.json describes the resolved dependency graph, which can\n'
                    'include dependencies unused by a particular platform. Supplemental\n'
                    'upstream texts are in third-party-licenses. This bundle is not itself\n'
                    'a legal review or a complete corresponding-source archive.\n')
                for name, data in sorted(selected.items()):
                    bundle.writestr(name, data)
    except Exception:
        if created:
            output.unlink(missing_ok=True)
        raise


if __name__ == '__main__':
    if len(sys.argv) != 3:
        sys.exit('usage: build_license_bundle.py SOURCE.tar.gz OUTPUT.zip')
    try:
        build(Path(sys.argv[1]), Path(sys.argv[2]))
    except (OSError, KeyError, ValueError, tarfile.TarError) as exc:
        sys.exit(str(exc))
