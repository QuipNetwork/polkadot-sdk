#!/usr/bin/env python3
"""
Script to parse changed_crates file and extract crate names with versions.
Extracts lines with 'name = "..."' and '+to = "..."' patterns and writes
the crate names with versions to a new file in format: - crate_name@version
"""

import re
import os
import argparse

def parse_crate_names(input_file, output_file):
    """
    Parse the input file to extract crate names with versions and write them to output file.

    Args:
        input_file (str): Path to the input file
        output_file (str): Path to the output file
    """
    crates = []

    # Pattern to match lines with name = "crate-name"
    name_pattern = r'name\s*=\s*"([^"]+)"'
    # Pattern to match lines with +to = "version"
    version_pattern = r'\+to\s*=\s*"([^"]+)"'

    try:
        with open(input_file, 'r', encoding='utf-8') as f:
            lines = f.readlines()

        current_crate = None
        for line_num, line in enumerate(lines, 1):
            # Look for lines that contain name = "something"
            name_match = re.search(name_pattern, line)
            if name_match:
                current_crate = name_match.group(1)
                print(f"Found crate name: {current_crate} (line {line_num})")

                # Look ahead for the +to version line
                # Typically it's within the next few lines
                for lookahead_offset in range(1, 10):
                    if line_num - 1 + lookahead_offset < len(lines):
                        version_line = lines[line_num - 1 + lookahead_offset]
                        version_match = re.search(version_pattern, version_line)
                        if version_match:
                            version = version_match.group(1)
                            crates.append((current_crate, version))
                            print(f"  -> Version: {version} (line {line_num + lookahead_offset})")
                            break

    except FileNotFoundError:
        print(f"Error: Input file '{input_file}' not found.")
        return
    except Exception as e:
        print(f"Error reading input file: {e}")
        return

    # Write crate names with versions to output file
    try:
        with open(output_file, 'w', encoding='utf-8') as f:
            f.write("The following crates were updated to the corresponding versions:\n\n")
            f.write("<details>\n")
            f.write("  <summary>Changed crates: </summary>\n\n")
            for crate_name, version in crates:
                f.write(f"- {crate_name}@{version}\n")
            f.write("\n</details>\n")
        print(f"\nSuccessfully extracted {len(crates)} crates with versions.")
        print(f"Output written to: {output_file}")

    except Exception as e:
        print(f"Error writing output file: {e}")

def main():
    parser = argparse.ArgumentParser(
        description='Parse changed_crates file and extract crate names.'
    )
    parser.add_argument(
        'input_file',
        help='Path to the input file containing crate information'
    )
    parser.add_argument(
        'output_file',
        help='Path to the output file where crate names will be written'
    )

    args = parser.parse_args()

    print("Parsing crate names from diff file...")
    print(f"Input file: {args.input_file}")
    print(f"Output file: {args.output_file}")
    print("-" * 50)

    parse_crate_names(args.input_file, args.output_file)

if __name__ == "__main__":
    main()
