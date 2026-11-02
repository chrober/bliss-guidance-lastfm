import io
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile


SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/build_license_bundle.py'


def add_file(tar, name, data):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    tar.addfile(info, io.BytesIO(data))


class LicenseBundleTests(unittest.TestCase):
    def test_fiat_authors_supplement_matches_upstream(self):
        path = SCRIPT.parents[1] / 'third-party-licenses/fiat-crypto-AUTHORS'
        self.assertTrue(path.is_file(), 'fiat-crypto authors notice is missing')
        content = path.read_text(encoding='utf-8').replace('\r\n', '\n').removesuffix('\n').encode('utf-8')
        self.assertEqual(hashlib.sha256(content).hexdigest(), 'ad74c036a6afaedc418fed65f171c877c2f74a4c70938a03930aa11c98a20508')

    def test_copies_inventory_vendor_notices_and_supplements(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / 'source.tar.gz', root / 'notices.zip'
            with tarfile.open(source, 'w:gz') as tar:
                add_file(tar, 'source/SOURCE_MANIFEST.json', json.dumps({'schema_version': 1, 'source_commit': 'a' * 40, 'packages': []}).encode())
                add_file(tar, 'source/vendor/example-1/LICENSE-MIT', b'license text')
                add_file(tar, 'source/vendor/example-1/nested/NOTICE', b'notice text')
                add_file(tar, 'source/vendor/example-1/AUTHORS', b'copyright and license')
                add_file(tar, 'source/vendor/example-1/src/lib.rs', b'not a notice')
                add_file(tar, 'source/third-party-licenses/missing-LICENSE', b'supplement')
            run = subprocess.run([sys.executable, str(SCRIPT), str(source), str(output)], capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            with zipfile.ZipFile(output) as bundle:
                self.assertEqual(bundle.read('vendor/example-1/LICENSE-MIT'), b'license text')
                self.assertEqual(bundle.read('vendor/example-1/nested/NOTICE'), b'notice text')
                self.assertEqual(bundle.read('vendor/example-1/AUTHORS'), b'copyright and license')
                self.assertEqual(bundle.read('third-party-licenses/missing-LICENSE'), b'supplement')
                self.assertIn('SOURCE_MANIFEST.json', bundle.namelist())
                self.assertNotIn('vendor/example-1/src/lib.rs', bundle.namelist())
            self.assertNotEqual(subprocess.run([sys.executable, str(SCRIPT), str(source), str(output)], capture_output=True).returncode, 0)

    def test_rejects_unsafe_members(self):
        for unsafe in ('source/vendor/a/../../LICENSE', 'source/vendor/a/..\\LICENSE'):
            with self.subTest(unsafe=unsafe), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source, output = root / 'source.tar.gz', root / 'notices.zip'
                with tarfile.open(source, 'w:gz') as tar:
                    add_file(tar, 'source/SOURCE_MANIFEST.json', b'{"schema_version":1,"packages":[]}')
                    add_file(tar, unsafe, b'bad')
                self.assertNotEqual(subprocess.run([sys.executable, str(SCRIPT), str(source), str(output)], capture_output=True).returncode, 0)
                self.assertFalse(output.exists())

    def test_includes_manifest_declared_license_with_nonstandard_name(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / 'source.tar.gz', root / 'notices.zip'
            declared = 'vendor/example-1/legal/terms.txt'
            manifest = {'schema_version': 1, 'packages': [{'notice_files': [declared]}]}
            with tarfile.open(source, 'w:gz') as tar:
                add_file(tar, 'source/SOURCE_MANIFEST.json', json.dumps(manifest).encode())
                add_file(tar, 'source/' + declared, b'license terms')
            run = subprocess.run([sys.executable, str(SCRIPT), str(source), str(output)], capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            with zipfile.ZipFile(output) as bundle:
                self.assertEqual(bundle.read(declared), b'license terms')

    def test_rejects_symlinked_notice_instead_of_silently_omitting_it(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / 'source.tar.gz', root / 'notices.zip'
            with tarfile.open(source, 'w:gz') as tar:
                add_file(tar, 'source/SOURCE_MANIFEST.json', b'{"schema_version":1,"packages":[]}')
                link = tarfile.TarInfo('source/vendor/example/LICENSE')
                link.type = tarfile.SYMTYPE
                link.linkname = 'elsewhere'
                tar.addfile(link)
            self.assertNotEqual(subprocess.run([sys.executable, str(SCRIPT), str(source), str(output)], capture_output=True).returncode, 0)
            self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
