#!/usr/bin/env python3
"""
Wrap every function in a file with /* */ block comment markers.

Algorithm:
  1. Find each function's "start line" - the first #[...] attribute immediately
     preceding the fn, or the fn line itself if no attributes.
  2. Insert  */\n/*  before each start line.
  3. Remove the first */  (so the file opens with /*  starting the first comment).
  4. Append */  as the final line (closes the last function's comment).

Result: imports/module-level code stay live; every function is commented out.
"""

import re
import sys

FN_PATTERN = re.compile(r'^\s*(pub\s+)?(async\s+)?fn\s+')
ATTR_PATTERN = re.compile(r'^\s*#\[')   # matches #[...] but NOT #![...]


def process_file(filepath):
    with open(filepath, 'r') as f:
        lines = f.readlines()

    if not lines:
        print(f"  {filepath}: empty, skipped")
        return

    # Find insertion points (line indices before which to insert */\n/*\n)
    insertion_points = []
    i = 0
    while i < len(lines):
        line = lines[i]

        if ATTR_PATTERN.match(line):
            # Collect the full contiguous attribute block
            block_start = i
            j = i
            while j < len(lines) and ATTR_PATTERN.match(lines[j]):
                j += 1
            # Only mark as insertion point if the block is immediately followed by fn
            if j < len(lines) and FN_PATTERN.match(lines[j]):
                insertion_points.append(block_start)
                i = j + 1   # skip past the fn definition line
                continue
            else:
                i = j       # skip past the attribute block
                continue

        if FN_PATTERN.match(line):
            # Bare fn with no preceding attributes
            insertion_points.append(i)

        i += 1

    if not insertion_points:
        print(f"  {filepath}: no functions found, skipped")
        return

    # Build new content: insert */\n/*\n before each insertion point
    new_lines = []
    prev = 0
    for idx in insertion_points:
        new_lines.extend(lines[prev:idx])
        new_lines.append('*/\n')
        new_lines.append('/*\n')
        prev = idx
    new_lines.extend(lines[prev:])

    # Remove the first */ to open an unclosed comment at the top
    for i, line in enumerate(new_lines):
        if line.rstrip('\n') == '*/':
            del new_lines[i]
            break

    # Append */ as the final line to close the last function's comment
    if new_lines and not new_lines[-1].endswith('\n'):
        new_lines[-1] += '\n'
    new_lines.append('*/\n')

    with open(filepath, 'w') as f:
        f.writelines(new_lines)

    print(f"  {filepath}: {len(insertion_points)} function(s) wrapped")


if __name__ == '__main__':
    files = sys.argv[1:]
    if not files:
        print("Usage: comment-out-fns.py <file1> [file2 ...]")
        sys.exit(1)

    for filepath in files:
        try:
            process_file(filepath)
        except Exception as e:
            print(f"ERROR processing {filepath}: {e}", file=sys.stderr)
            sys.exit(1)
