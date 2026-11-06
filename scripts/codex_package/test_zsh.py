import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package.targets import TARGET_SPECS
from codex_package.zsh import resolve_zsh_bin


class ZshOverrideTests(unittest.TestCase):
    def test_local_override_skips_prebuilt_download(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "zsh.exe"
            executable.write_bytes(b"local patched zsh")
            executable.chmod(0o755)
            with patch("codex_package.zsh.fetch_dotslash_executable") as fetch:
                self.assertEqual(
                    resolve_zsh_bin(
                        TARGET_SPECS["x86_64-unknown-linux-musl"], executable
                    ),
                    executable.resolve(),
                )
                fetch.assert_not_called()
