"""Check the Node bridge output against Python's independent ZIP reader."""
import io
import pathlib
import subprocess
import sys
import zipfile

binary = pathlib.Path(sys.argv[1]).resolve()
module = pathlib.Path(__file__).resolve().parents[2] / "archive.js"
bridge = """
const {archiveFiles} = await import(process.argv[1])
async function* originals() {
  yield {path:'receipts/évidence.txt', bytes:Buffer.from('revision two')}
  yield {path:'notes/empty.txt', bytes:Buffer.alloc(0)}
}
process.stdout.write(await archiveFiles(originals(), process.argv[2]))
"""
result = subprocess.run(
    ["node", "--input-type=module", "-e", bridge, module.as_uri(), str(binary)],
    check=True, capture_output=True,
)
with zipfile.ZipFile(io.BytesIO(result.stdout)) as archive:
    assert archive.testzip() is None
    assert archive.namelist() == ["receipts/évidence.txt", "notes/empty.txt"]
    assert archive.read("receipts/évidence.txt") == b"revision two"
    assert archive.read("notes/empty.txt") == b""
truncated = subprocess.run([str(binary)], input=b"\x01", capture_output=True)
assert truncated.returncode and not truncated.stdout
print("ZIP paths, UTF-8 names, contents, CRCs and truncated-input behavior passed.")
