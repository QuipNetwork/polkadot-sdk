#!/usr/bin/env python3

import os
import sys
import json
import argparse
import _help
import importlib.util
import re
import difflib

_HelpAction = _help._HelpAction

_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.normpath(os.path.join(_SCRIPT_DIR, '..', '..', '..'))

f = open(os.path.join(_REPO_ROOT, '.github/workflows/runtimes-matrix.json'), 'r')
runtimesMatrix = json.load(f)

runtimeNames = list(map(lambda x: x['name'], runtimesMatrix))

common_args = {
    '--quiet': {"action": "store_true", "help": "Won't print start/end/failed messages in PR"},
    '--clean': {"action": "store_true", "help": "Clean up the previous bot's & author's comments in PR"},
    '--image': {"help": "Override docker image '--image docker.io/paritytech/ci-unified:latest'"},
}

def print_and_log(message, output_file='/tmp/cmd/command_output.log'):
    print(message)
    with open(output_file, 'a') as f:
        f.write(message + '\n')

def setup_logging():
    if not os.path.exists('/tmp/cmd'):
        os.makedirs('/tmp/cmd')
    open('/tmp/cmd/command_output.log', 'w')

def fetch_repo_labels():
    """Fetch labels from environment (pre-fetched by workflow)"""
    labels_json = os.environ.get('REPO_LABELS')
    if labels_json:
        try:
            labels = json.loads(labels_json)
            print_and_log(f"Loaded {len(labels)} labels from environment")
            return labels
        except json.JSONDecodeError as e:
            print_and_log(f"Error parsing REPO_LABELS: {e}")
            return None
    else:
        print_and_log("REPO_LABELS not set in environment")
        return None


def check_pr_status(pr_number):
    """Check PR status from environment (pre-fetched by workflow)"""
    status_json = os.environ.get('PR_STATUS')
    if not status_json:
        print_and_log("Error: PR_STATUS not set, cannot verify PR status")
        return False

    try:
        data = json.loads(status_json)
        if data.get('merged', False):
            return False
        if data.get('state') == 'closed':
            return False
        if data.get('auto_merge', False):
            return False
        return True
    except json.JSONDecodeError as e:
        print_and_log(f"Error parsing PR_STATUS: {e}")
        return False


def find_closest_labels(invalid_label, valid_labels, max_suggestions=3, cutoff=0.6):
    """Find the closest matching labels using fuzzy string matching"""
    # Get close matches using difflib
    close_matches = difflib.get_close_matches(
        invalid_label,
        valid_labels,
        n=max_suggestions,
        cutoff=cutoff
    )

    return close_matches

def auto_correct_labels(invalid_labels, valid_labels, auto_correct_threshold=0.8):
    """Automatically correct labels when confidence is high, otherwise suggest"""
    corrections = []
    suggestions = []

    for invalid_label in invalid_labels:
        closest = find_closest_labels(invalid_label, valid_labels, max_suggestions=1)

        if closest:
            # Calculate similarity for the top match
            top_match = closest[0]
            similarity = difflib.SequenceMatcher(None, invalid_label.lower(), top_match.lower()).ratio()

            if similarity >= auto_correct_threshold:
                # High confidence - auto-correct
                corrections.append((invalid_label, top_match))
            else:
                # Lower confidence - suggest alternatives
                all_matches = find_closest_labels(invalid_label, valid_labels, max_suggestions=3)
                if all_matches:
                    labels_str = ', '.join(f"'{label}'" for label in all_matches)
                    suggestion = f"'{invalid_label}' → did you mean: {labels_str}?"
                else:
                    suggestion = f"'{invalid_label}' → no close matches found"
                suggestions.append(suggestion)
        else:
            # No close matches - try prefix suggestions
            prefix_match = re.match(r'^([A-Z]\d+)-', invalid_label)
            if prefix_match:
                prefix = prefix_match.group(1)
                prefix_labels = [label for label in valid_labels if label.startswith(prefix + '-')]
                if prefix_labels:
                    # If there's exactly one prefix match, auto-correct it
                    if len(prefix_labels) == 1:
                        corrections.append((invalid_label, prefix_labels[0]))
                    else:
                        # Multiple prefix matches - suggest alternatives
                        suggestion = f"'{invalid_label}' → try labels starting with '{prefix}-': {', '.join(prefix_labels[:3])}"
                        suggestions.append(suggestion)
                else:
                    suggestion = f"'{invalid_label}' → no labels found with prefix '{prefix}-'"
                    suggestions.append(suggestion)
            else:
                suggestion = f"'{invalid_label}' → invalid format (expected format: 'T1-FRAME', 'I2-bug', etc.)"
                suggestions.append(suggestion)

    return corrections, suggestions

parser = argparse.ArgumentParser(prog="/cmd ", description='A command runner for polkadot-sdk repo', add_help=False)
parser.add_argument('--help', action=_HelpAction, help='help for help if you need some help')  # help for help
for arg, config in common_args.items():
    parser.add_argument(arg, **config)

subparsers = parser.add_subparsers(help='a command to run', dest='command')

setup_logging()

"""
BENCH
"""

bench_example = '''**Examples**:
 Runs all benchmarks 
 %(prog)s

 Runs benchmarks for pallet_balances and pallet_multisig for all runtimes which have these pallets. **--quiet** makes it to output nothing to PR but reactions
 %(prog)s --pallet pallet_balances pallet_xcm_benchmarks::generic --quiet
 
 Runs bench for all pallets for westend runtime and fails fast on first failed benchmark
 %(prog)s --runtime westend --fail-fast
 
 Does not output anything and cleans up the previous bot's & author command triggering comments in PR 
 %(prog)s --runtime westend --pallet pallet_balances pallet_multisig --quiet --clean
'''

parser_bench = subparsers.add_parser('bench', aliases=['bench-omni'], help='Runs benchmarks (frame omni bencher)', epilog=bench_example, formatter_class=argparse.RawDescriptionHelpFormatter)

for arg, config in common_args.items():
    parser_bench.add_argument(arg, **config)

parser_bench.add_argument('--runtime', help='Runtime(s) space separated', choices=runtimeNames, nargs='*', default=runtimeNames)
parser_bench.add_argument('--pallet', help='Pallet(s) space separated', nargs='*', default=[])
parser_bench.add_argument('--fail-fast', help='Fail fast on first failed benchmark', action='store_true')


"""
FMT
"""
parser_fmt = subparsers.add_parser('fmt', help='Formats code (cargo +nightly-VERSION fmt) and configs (taplo format)')
for arg, config in common_args.items():
    parser_fmt.add_argument(arg, **config)

"""
Update UI
"""
parser_ui = subparsers.add_parser('update-ui', help='Updates UI tests')
for arg, config in common_args.items():
    parser_ui.add_argument(arg, **config)

"""
PRDOC
"""
# Import generate-prdoc.py dynamically
spec = importlib.util.spec_from_file_location("generate_prdoc", os.path.join(_REPO_ROOT, ".github/scripts/generate-prdoc.py"))
generate_prdoc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generate_prdoc)

parser_prdoc = subparsers.add_parser('prdoc', help='Generates PR documentation')
generate_prdoc.setup_parser(parser_prdoc, pr_required=False)

"""
LABEL
"""
# Fetch current labels from repository
def get_allowed_labels():
    """Get the current list of allowed labels"""
    repo_labels = fetch_repo_labels()

    if repo_labels is not None:
        return repo_labels
    else:
        # Fail if API fetch fails
        raise RuntimeError("Failed to fetch labels from repository. Please check your connection and try again.")

def validate_and_auto_correct_labels(input_labels, valid_labels):
    """Validate labels and auto-correct when confidence is high"""
    final_labels = []
    correction_messages = []
    all_suggestions = []
    no_match_labels = []

    # Process all labels first to collect all issues
    for label in input_labels:
        if label in valid_labels:
            final_labels.append(label)
        else:
            # Invalid label - try auto-correction
            corrections, suggestions = auto_correct_labels([label], valid_labels)

            if corrections:
                # Auto-correct with high confidence
                original, corrected = corrections[0]
                final_labels.append(corrected)
                similarity = difflib.SequenceMatcher(None, original.lower(), corrected.lower()).ratio()
                correction_messages.append(f"Auto-corrected '{original}' → '{corrected}' (similarity: {similarity:.2f})")
            elif suggestions:
                # Low confidence - collect for batch error
                all_suggestions.extend(suggestions)
            else:
                # No suggestions at all
                no_match_labels.append(label)

    # If there are any labels that couldn't be auto-corrected, show all at once
    if all_suggestions or no_match_labels:
        error_parts = []

        if all_suggestions:
            error_parts.append("Labels requiring manual selection:")
            for suggestion in all_suggestions:
                error_parts.append(f"  • {suggestion}")

        if no_match_labels:
            if all_suggestions:
                error_parts.append("")  # Empty line for separation
            error_parts.append("Labels with no close matches:")
            for label in no_match_labels:
                error_parts.append(f"  • '{label}' → no valid suggestions available")

        error_parts.append("")
        error_parts.append("For all available labels, see: https://paritytech.github.io/labels/doc_polkadot-sdk.html")

        error_msg = "\n".join(error_parts)
        raise ValueError(error_msg)

    return final_labels, correction_messages

label_example = '''**Examples**:
 Add single label
 %(prog)s T1-FRAME

 Add multiple labels
 %(prog)s T1-FRAME R0-no-crate-publish-required

 Add multiple labels
 %(prog)s T1-FRAME A2-substantial D3-involved

Labels are fetched dynamically from the repository.
Typos are auto-corrected when confidence is high (>80%% similarity).
For label meanings, see: https://paritytech.github.io/labels/doc_polkadot-sdk.html
'''

parser_label = subparsers.add_parser('label', help='Add labels to PR (self-service for contributors)', epilog=label_example, formatter_class=argparse.RawDescriptionHelpFormatter)
for arg, config in common_args.items():
    parser_label.add_argument(arg, **config)

parser_label.add_argument('labels', nargs='+', help='Labels to add to the PR (auto-corrects typos)')

def main():
    global args, unknown, runtimesMatrix
    args, unknown = parser.parse_known_args()

    print(f'args: {args}')

    if args.command == 'bench' or args.command == 'bench-omni':
        runtime_pallets_map = {}
        failed_benchmarks = {}
        successful_benchmarks = {}

        profile = "production"

        print(f'Provided runtimes: {args.runtime}')
        # convert to mapped dict
        runtimesMatrix = list(filter(lambda x: x['name'] in args.runtime, runtimesMatrix))
        runtimesMatrix = {x['name']: x for x in runtimesMatrix}
        print(f'Filtered out runtimes: {runtimesMatrix}')

        compile_bencher = os.system(f"cargo install -q --path substrate/utils/frame/omni-bencher --locked --profile {profile}")
        if compile_bencher != 0:
            print_and_log('❌ Failed to compile frame-omni-bencher')
            sys.exit(1)

        # loop over remaining runtimes to collect available pallets
        for runtime in runtimesMatrix.values():
            build_command = f"forklift cargo build -q -p {runtime['package']} --profile {profile} --features={runtime['bench_features']}"
            print(f'-- building "{runtime["name"]}" with `{build_command}`')
            build_status = os.system(build_command)
            if build_status != 0:
                print_and_log(f'❌ Failed to build {runtime["name"]}')
                if args.fail_fast:
                    sys.exit(1)
                else:
                    continue

            print(f'-- listing pallets for benchmark for {runtime["name"]}')
            wasm_file = f"target/{profile}/wbuild/{runtime['package']}/{runtime['package'].replace('-', '_')}.wasm"
            list_command = f"frame-omni-bencher v1 benchmark pallet " \
                f"--no-csv-header " \
                f"--no-storage-info " \
                f"--no-min-squares " \
                f"--no-median-slopes " \
                f"--all " \
                f"--list " \
                f"--runtime={wasm_file} " \
                f"{runtime['bench_flags']}"
            print(f'-- running: {list_command}')
            output = os.popen(list_command).read()
            raw_pallets = output.strip().split('\n')

            all_pallets = set()
            for pallet in raw_pallets:
                if pallet:
                    all_pallets.add(pallet.split(',')[0].strip())

            pallets = list(all_pallets)
            print(f'Pallets in {runtime["name"]}: {pallets}')
            runtime_pallets_map[runtime['name']] = pallets

        print(f'\n')

        # filter out only the specified pallets from collected runtimes/pallets
        if args.pallet:
            print(f'Pallets: {args.pallet}')
            new_pallets_map = {}
            # keep only specified pallets if they exist in the runtime
            for runtime in runtime_pallets_map:
                if set(args.pallet).issubset(set(runtime_pallets_map[runtime])):
                    new_pallets_map[runtime] = args.pallet

            runtime_pallets_map = new_pallets_map

        print(f'Filtered out runtimes & pallets: {runtime_pallets_map}\n')

        if not runtime_pallets_map:
            if args.pallet and not args.runtime:
                print(f"No pallets {args.pallet} found in any runtime")
            elif args.runtime and not args.pallet:
                print(f"{args.runtime} runtime does not have any pallets")
            elif args.runtime and args.pallet:
                print(f"No pallets {args.pallet} found in {args.runtime}")
            else:
                print('No runtimes found')
            sys.exit(1)

        for runtime in runtime_pallets_map:
            for pallet in runtime_pallets_map[runtime]:
                config = runtimesMatrix[runtime]
                header_path = os.path.abspath(config['header'])
                template = None

                print(f'-- config: {config}')
                if runtime == 'dev':
                    # to support sub-modules (https://github.com/paritytech/command-bot/issues/275)
                    search_manifest_path = f"cargo metadata --locked --format-version 1 --no-deps | jq -r '.packages[] | select(.name == \"{pallet.replace('_', '-')}\") | .manifest_path'"
                    print(f'-- running: {search_manifest_path}')
                    manifest_path = os.popen(search_manifest_path).read()
                    if not manifest_path:
                        print(f'-- pallet {pallet} not found in dev runtime')
                        if args.fail_fast:
                            print_and_log(f'Error: {pallet} not found in dev runtime')
                            sys.exit(1)
                    package_dir = os.path.dirname(manifest_path)
                    print(f'-- package_dir: {package_dir}')
                    print(f'-- manifest_path: {manifest_path}')
                    output_path = os.path.join(package_dir, "src", "weights.rs")
                    # TODO: we can remove once all pallets in dev runtime are migrated to polkadot-sdk-frame
                    try:
                        uses_polkadot_sdk_frame = "true" in os.popen(f"cargo metadata --locked --format-version 1 --no-deps | jq -r '.packages[] | select(.name == \"{pallet.replace('_', '-')}\") | .dependencies | any(.name == \"polkadot-sdk-frame\")'").read()
                        print(f'uses_polkadot_sdk_frame: {uses_polkadot_sdk_frame}')
                    # Empty output from the previous os.popen command
                    except StopIteration:
                        print(f'Error: {pallet} not found in dev runtime')
                        uses_polkadot_sdk_frame = False
                    template = config['template']
                    if uses_polkadot_sdk_frame and re.match(r"frame-(:?umbrella-)?weight-template\.hbs", os.path.normpath(template).split(os.path.sep)[-1]):
                        template = "substrate/.maintain/frame-umbrella-weight-template.hbs"
                    print(f'template: {template}')
                else:
                    default_path = f"./{config['path']}/src/weights"
                    xcm_path = f"./{config['path']}/src/weights/xcm"
                    output_path = default_path
                    if pallet.startswith("pallet_xcm_benchmarks"):
                        template = config['template']
                        output_path = xcm_path

                print(f'-- benchmarking {pallet} in {runtime} into {output_path}')
                cmd = f"frame-omni-bencher v1 benchmark pallet " \
                    f"--extrinsic=* " \
                    f"--runtime=target/{profile}/wbuild/{config['package']}/{config['package'].replace('-', '_')}.wasm " \
                    f"--pallet={pallet} " \
                    f"--header={header_path} " \
                    f"--output={output_path} " \
                    f"--wasm-execution=compiled " \
                    f"--steps=50 " \
                    f"--repeat=20 " \
                    f"--heap-pages=4096 " \
                    f"{f'--template={template} ' if template else ''}" \
                    f"--no-storage-info --no-min-squares --no-median-slopes " \
                    f"{config['bench_flags']}"
                print(f'-- Running: {cmd} \n')
                os.environ['RUNTIME_LOG'] = 'off' # Turn off annoying logs during benchmarking
                status = os.system(cmd)

                if status != 0 and args.fail_fast:
                    print_and_log(f'❌ Failed to benchmark {pallet} in {runtime}')
                    sys.exit(1)

                # Otherwise collect failed benchmarks and print them at the end
                # push failed pallets to failed_benchmarks
                if status != 0:
                    failed_benchmarks[f'{runtime}'] = failed_benchmarks.get(f'{runtime}', []) + [pallet]
                else:
                    successful_benchmarks[f'{runtime}'] = successful_benchmarks.get(f'{runtime}', []) + [pallet]

        if failed_benchmarks:
            print_and_log('❌ Failed benchmarks of runtimes/pallets:')
            for runtime, pallets in failed_benchmarks.items():
                print_and_log(f'-- {runtime}: {pallets}')

        if successful_benchmarks:
            print_and_log('✅ Successful benchmarks of runtimes/pallets:')
            for runtime, pallets in successful_benchmarks.items():
                print_and_log(f'-- {runtime}: {pallets}')

    elif args.command == 'fmt':
        command = f"cargo +nightly fmt"
        print(f'Formatting with `{command}`')
        nightly_status = os.system(f'{command}')
        taplo_status = os.system('taplo format --config .config/taplo.toml')

        if (nightly_status != 0 or taplo_status != 0):
            print_and_log('❌ Failed to format code')
            sys.exit(1)

    elif args.command == 'update-ui':
        command = 'sh ./scripts/update-ui-tests.sh'
        print(f'Updating ui with `{command}`')
        status = os.system(f'{command}')

        if status != 0:
            print_and_log('❌ Failed to update ui')
            sys.exit(1)

    elif args.command == 'prdoc':
        # Call the main function from ./github/scripts/generate-prdoc.py module
        exit_code = generate_prdoc.main(args)
        if exit_code != 0:
            print_and_log('❌ Failed to generate prdoc')
            sys.exit(exit_code)

    elif args.command == 'label':
        # The actual labeling is handled by the GitHub Action workflow
        # This script validates and auto-corrects labels

        try:
            # Check if PR is still open and not merged/in merge queue
            pr_number = os.environ.get('PR_NUM')
            if pr_number:
                if not check_pr_status(pr_number):
                    raise ValueError("Cannot modify labels on merged PRs or PRs in merge queue")

            # Check if user has permission to modify labels
            is_org_member = os.environ.get('IS_ORG_MEMBER', 'false').lower() == 'true'
            is_pr_author = os.environ.get('IS_PR_AUTHOR', 'false').lower() == 'true'

            if not is_org_member and not is_pr_author:
                raise ValueError("Only the PR author or organization members can modify labels")

            # Get allowed labels dynamically
            try:
                allowed_labels = get_allowed_labels()
            except RuntimeError as e:
                raise ValueError(str(e))

            # Validate and auto-correct labels
            final_labels, correction_messages = validate_and_auto_correct_labels(args.labels, allowed_labels)

            # Show auto-correction messages
            for message in correction_messages:
                print(message)

            # Output labels as JSON for GitHub Action
            import json
            labels_output = {"labels": final_labels}
            print_and_log(f"LABELS_JSON: {json.dumps(labels_output)}")
        except ValueError as e:
            print_and_log(f'❌ {e}')

            # Output error as JSON for GitHub Action
            import json
            error_output = {
                "error": "validation_failed",
                "message": "Invalid labels found. Please check the suggestions below and try again.",
                "details": str(e)
            }
            print(f"ERROR_JSON: {json.dumps(error_output)}")
            sys.exit(1)

    print('🚀 Done')

if __name__ == '__main__':
    main()
