import os
import subprocess
import sys


def test_core_imports_without_beets():
    # The slim _core must depend only on musefs_common, never on beets itself.
    # Prove it: import beetsplug._core in a fresh interpreter with `beets`
    # masked (sys.modules['beets'] = None makes any `import beets` raise), so an
    # accidental beets import anywhere in the chain fails the subprocess. Pass
    # the current sys.path through so the child resolves the same packages.
    code = (
        "import sys; sys.modules['beets'] = None; "
        "import beetsplug._core as c; "
        "assert c.RENAME and c.map_fields and c.build_records"
    )
    env = {**os.environ, "PYTHONPATH": os.pathsep.join(p for p in sys.path if p)}
    subprocess.run([sys.executable, "-c", code], check=True, env=env)
