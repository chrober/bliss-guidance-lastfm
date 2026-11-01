#!/usr/bin/env python3
"""Create a vendored source snapshot for an explicitly selected Git commit.

The archive is evidence of source contents, not proof that a published binary
was built from that commit or that all third-party rights have been cleared.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile


def command(args, cwd, **kwargs):
    return subprocess.run(args, cwd=cwd, check=True, **kwargs)


def create_archive(repo: Path, revision: str, output: Path) -> None:
    if output.exists():
        raise FileExistsError(f'refusing to overwrite {output}')
    repo = repo.resolve(strict=True)
    sha = command(['git', 'rev-parse', '--verify', f'{revision}^{{commit}}'], repo,
                  text=True, capture_output=True).stdout.strip()
    tree = command(['git', 'ls-tree', '-r', sha], repo, text=True, capture_output=True).stdout
    if any(line.startswith('160000 ') for line in tree.splitlines()):
        raise ValueError('Git submodules require explicit source capture; refusing incomplete archive')
    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='bliss-source-') as temp:
        temporary = Path(temp)
        source = temporary / 'source'
        source.mkdir()
        archive_path = temporary / 'git-tree.tar'
        with archive_path.open('wb') as stream:
            command(['git', 'archive', '--format=tar', sha], repo, stdout=stream)
        with tarfile.open(archive_path, 'r:') as archive:
            archive.extractall(source, filter='data')
        if not (source / 'Cargo.lock').is_file():
            raise ValueError('source tree has no Cargo.lock')
        config_dir = source / '.cargo'
        if (config_dir / 'config').exists() or (config_dir / 'config.toml').exists():
            raise ValueError('existing Cargo configuration requires explicit review')
        vendor = command(['cargo', 'vendor', '--locked', '--versioned-dirs', 'vendor'], source,
                         text=True, capture_output=True)
        config_dir.mkdir(exist_ok=True)
        (config_dir / 'config.toml').write_text(vendor.stdout, encoding='utf-8')
        metadata = command(['cargo', 'metadata', '--locked', '--offline', '--format-version', '1'],
                           source, text=True, capture_output=True)
        packages = json.loads(metadata.stdout)['packages']
        def source_path(value, base=None):
            if value is None:
                return None
            path = Path(value)
            if not path.is_absolute() and base is not None:
                path = Path(base).parent / path
            return path.resolve().relative_to(source.resolve()).as_posix()

        def notice_files(package):
            root = Path(package['manifest_path']).parent
            files = {source_path(path) for path in root.iterdir()
                     if path.is_file() and path.name.upper().startswith(('LICENSE', 'COPYING', 'NOTICE', 'COPYRIGHT', 'AUTHORS'))}
            if package['license_file'] is not None:
                declared = source_path(package['license_file'], package['manifest_path'])
                if not (source / declared).is_file():
                    raise ValueError(f"declared license file missing for {package['name']}: {declared}")
                files.add(declared)
            return sorted(files)

        inventory = [{'name': p['name'], 'version': p['version'], 'license': p['license'],
                      'license_file': source_path(p['license_file'], p['manifest_path']),
                      'manifest_path': source_path(p['manifest_path']),
                      'notice_files': notice_files(p), 'source': p['source'],
                      'repository': p['repository']} for p in packages]
        manifest = {'schema_version': 1, 'source_commit': sha, 'packages': inventory,
                    'note': 'Package metadata is not a legal review or binary build attestation.'}
        (source / 'SOURCE_MANIFEST.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        (source / 'SOURCE_ARCHIVE_README.txt').write_text(
            'This archive captures a Git commit and Cargo dependencies. The vendor directory\n'
            'contains crate source and included notices. See SOURCE_MANIFEST.json and the\n'
            'project workflows for build details. Try cargo build --locked --offline after\n'
            'installing the required Rust toolchain and platform-specific build tools.\n'
            'Binary-to-source provenance and third-party rights require separate verification.\n',
            encoding='utf-8')
        created = False
        try:
            with output.open('xb') as stream:
                created = True
                with tarfile.open(fileobj=stream, mode='w:gz') as result:
                    result.add(source, arcname='source')
        except Exception:
            if created:
                output.unlink(missing_ok=True)
            raise


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--commit', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    create_archive(args.repo, args.commit, args.output)
