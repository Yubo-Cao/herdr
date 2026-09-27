import hashlib
import tempfile
import unittest
from pathlib import Path

from scripts.thyra_distribution import build_manifest


class ThyraDistributionTests(unittest.TestCase):
    def test_manifest_pins_all_platforms_to_tag_and_hashes_built_files(self):
        names = {
            "linux-x86_64": "herdr-linux-x86_64",
            "linux-aarch64": "herdr-linux-aarch64",
            "macos-x86_64": "herdr-macos-x86_64",
            "macos-aarch64": "herdr-macos-aarch64",
            "windows-x86_64": "herdr-windows-x86_64.zip",
        }
        with tempfile.TemporaryDirectory() as tmp:
            assets = Path(tmp)
            for name in names.values():
                (assets / name).write_bytes(f"built {name}\n".encode())
            manifest = build_manifest("Yubo-Cao/herdr", "v0.9.1-thyra.2", assets)
            self.assertEqual(manifest["version"], "0.9.1-thyra.2")
            self.assertTrue(manifest["notes"].strip())
            self.assertIsInstance(manifest["protocol"], int)
            self.assertIsInstance(manifest["endpoint_generation"], int)
            self.assertEqual(set(manifest["assets"]), set(names))
            self.assertEqual(set(manifest["sha256"]), set(names))
            for target, name in names.items():
                self.assertEqual(
                    manifest["assets"][target],
                    f"https://github.com/Yubo-Cao/herdr/releases/download/v0.9.1-thyra.2/{name}",
                )
                self.assertEqual(
                    manifest["sha256"][target],
                    hashlib.sha256(f"built {name}\n".encode()).hexdigest(),
                )

    def test_missing_asset_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(FileNotFoundError):
                build_manifest("Yubo-Cao/herdr", "v0.9.1-thyra.1", Path(tmp))

    def test_non_thyra_tags_fail(self):
        for tag in ("v0.9.1", "0.9.1-thyra.1", "v0.9.1-thyra.x", "v0.9.1-thyra.1/other"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                build_manifest("Yubo-Cao/herdr", tag, Path("unused"))


if __name__ == "__main__":
    unittest.main()
