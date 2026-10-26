import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('build',Path(__file__).resolve().parents[1]/'host/build.py')
build=importlib.util.module_from_spec(spec);spec.loader.exec_module(build)

class StreamingBuildTests(unittest.TestCase):
    def test_export_digest_and_extracted_content_match(self):
        payload=b'public-fixture\0'*(1<<17)
        stream=io.BytesIO()
        with tarfile.open(fileobj=stream,mode='w') as archive:
            entry=tarfile.TarInfo('fixture');entry.size=len(payload);archive.addfile(entry,io.BytesIO(payload))
        archive_bytes=stream.getvalue();stream.seek(0)
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(build.unpack_stream(stream,Path(directory)),hashlib.sha256(archive_bytes).hexdigest())
            self.assertEqual((Path(directory)/'fixture').read_bytes(),payload)
            self.assertEqual(sorted(p.name for p in Path(directory).iterdir()),['fixture'])
    def test_corrupt_archive_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises((RuntimeError,BrokenPipeError)):
                build.unpack_stream(io.BytesIO(b'not a tar archive'),Path(directory))
    def test_model_link_reuses_bytes_and_preserves_existing_source(self):
        with tempfile.TemporaryDirectory() as directory:
            source=Path(directory)/'source';target=Path(directory)/'target';source.write_bytes(b'approved public model')
            build.install_volume(source,target,True)
            self.assertEqual(source.stat().st_ino,target.stat().st_ino)
            build.install_volume(source,target,False)
            self.assertEqual(source.read_bytes(),b'approved public model')
    def test_existing_different_volume_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            source=Path(directory)/'source';target=Path(directory)/'target';source.write_bytes(b'new');target.write_bytes(b'existing')
            with self.assertRaisesRegex(ValueError,'differs'):build.install_volume(source,target,True)
            self.assertEqual(target.read_bytes(),b'existing')

if __name__=='__main__':unittest.main()
