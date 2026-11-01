"""Exercise source bundling using real Git and Cargo, without a network service."""
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts' / 'build_source_archive.py'


def run(*args, cwd=None, env=None):
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)
    if result.returncode:
        raise AssertionError(f'{args}: {result.stdout}\n{result.stderr}')
    return result


class SourceArchiveTests(unittest.TestCase):
    def test_archives_committed_tree_and_vendored_git_dependency_for_offline_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dep, project = root / 'dependency', root / 'project'
            for directory in (dep, project):
                (directory / 'src').mkdir(parents=True)
                run('git', 'init', '-q', cwd=directory)
                run('git', 'config', 'user.name', 'Fixture', cwd=directory)
                run('git', 'config', 'user.email', 'fixture@example.invalid', cwd=directory)
            (dep / 'Cargo.toml').write_text('[package]\nname="fixture-dep"\nversion="0.1.0"\nedition="2021"\nlicense-file="LICENSE"\n')
            (dep / 'LICENSE').write_text('Fixture license notice.\n')
            (dep / 'AUTHORS').write_text('Additional copyright and license material.\n')
            (dep / 'src/lib.rs').write_text('pub fn value() -> u32 { 42 }\n')
            run('git', 'add', '.', cwd=dep)
            run('git', 'commit', '-qm', 'dependency', cwd=dep)
            (project / 'Cargo.toml').write_text('[package]\nname="fixture-app"\nversion="0.1.0"\nedition="2021"\nlicense="GPL-3.0-only"\n[dependencies]\nfixture-dep={git="' + dep.as_uri() + '"}\n')
            (project / 'src/main.rs').write_text('fn main() { println!("{}", fixture_dep::value()); }\n')
            run('cargo', 'generate-lockfile', cwd=project)
            run('git', 'add', '.', cwd=project)
            run('git', 'commit', '-qm', 'project', cwd=project)
            commit = run('git', 'rev-parse', 'HEAD', cwd=project).stdout.strip()
            (project / 'src/main.rs').write_text('uncommitted change, must not be archived\n')
            archive = root / 'source.tar.gz'
            run('python', str(SCRIPT), '--repo', str(project), '--commit', commit, '--output', str(archive))
            extracted = root / 'extracted'
            with tarfile.open(archive, 'r:gz') as tar:
                tar.extractall(extracted, filter='data')
            source = extracted / 'source'
            self.assertIn('fixture_dep::value()', (source / 'src/main.rs').read_text())
            self.assertTrue((source / 'vendor/fixture-dep-0.1.0/src/lib.rs').is_file())
            manifest = json.loads((source / 'SOURCE_MANIFEST.json').read_text())
            self.assertEqual(manifest['source_commit'], commit)
            dependency = next(p for p in manifest['packages'] if p['name'] == 'fixture-dep')
            self.assertIn('fixture-dep-0.1.0', dependency['manifest_path'])
            self.assertEqual((source / dependency['license_file']).read_text(), 'Fixture license notice.\n')
            self.assertIn(dependency['license_file'], dependency['notice_files'])
            self.assertIn('vendor/fixture-dep-0.1.0/AUTHORS', dependency['notice_files'])
            env = dict(os.environ, CARGO_HOME=str(root / 'empty-cargo-home'))
            run('cargo', 'check', '--locked', '--offline', cwd=source, env=env)

    def test_refuses_to_overwrite_an_existing_archive(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'source.tar.gz'
            output.write_bytes(b'keep')
            result = subprocess.run(['python', str(SCRIPT), '--repo', tmp, '--commit', 'HEAD', '--output', str(output)], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(output.read_bytes(), b'keep')


if __name__ == '__main__':
    unittest.main()
