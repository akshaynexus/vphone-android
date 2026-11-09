#!/usr/bin/env python3
"""Make the composed Linux tree compile for Android.

os(Linux) code paths become os(Linux) || os(Android), and the Glibc module becomes bionic's Android
module. edits.txt then applies exact, checked replacements (every line must change its file)."""
import pathlib, re, sys

tree = pathlib.Path(sys.argv[1]).resolve()
vendor = pathlib.Path(sys.argv[2]).resolve()
here = pathlib.Path(__file__).resolve().parent

def transform(text):
    t = re.sub(r'os\(Linux\)', '(os(Linux) || os(Android))', text)
    t = re.sub(r'canImport\(Glibc\)', 'canImport(Android)', t)
    t = re.sub(r'^(\s*(?:@preconcurrency |@_exported |@_implementationOnly )*import )Glibc\b', r'\1Android', t, flags=re.M)
    t = re.sub(r'\bGlibc\.', 'Android.', t)
    # Bionic's S_IF* exist in both the Android overlay and the posix_filesystem clang module.
    t = re.sub(r'(?<![\w.])S_IF(MT|DIR|REG|LNK|CHR|BLK|IFO|SOCK)\b', r'Android.S_IF\1', t)
    return t

roots = [tree / 'Sources', tree / 'Shims', tree / 'Vendor', vendor / 'MachOKit' / 'Sources']
for root in roots:
    for p in root.rglob('*.swift'):
        original = p.read_text(errors='surrogateescape')
        new = transform(original)
        if new != original:
            if p.is_symlink():
                p.unlink()
            p.write_text(new, errors='surrogateescape')

edits = here / 'edits.txt'
if edits.exists():
    for line in edits.read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        rel, pattern, repl = line.split('\t', 2)
        repl = repl.replace('\\n', '\n')
        p = (vendor / rel[8:]) if rel.startswith('@vendor/') else tree / rel
        original = p.read_text(errors='surrogateescape')
        new, n = re.subn(pattern, repl, original, flags=re.M)
        if n == 0:
            sys.exit(f"edit did not apply: {rel}: {pattern}")
        if p.is_symlink():
            p.unlink()
        p.write_text(new, errors='surrogateescape')
