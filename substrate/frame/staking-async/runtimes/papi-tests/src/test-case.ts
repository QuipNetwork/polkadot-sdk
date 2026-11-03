import { readFileSync } from "fs";
import { logger, safeJsonStringify, type ApiDeclarations } from "./utils";
import { exit } from "process";
import chalk from "chalk";

export enum Chain {
	Relay = "Rely",
	Parachain = "Para",
}

interface IEvent {
	module: string;
	event: string;
	data: any | undefined;
}

interface IBlock {
	chain: Chain;
	number: number;
	hash: string;
	events: IEvent[];
	weights: any;
	authorship: IAuthorshipData | null;
}

/// The on-chain weight consumed in a block, exactly as stored by `frame-system`
interface IWeight {
	normal: {
		ref_time: bigint;
		proof_size: bigint;
	};
	operational: {
		ref_time: bigint;
		proof_size: bigint;
	};
	mandatory: {
		ref_time: bigint;
		proof_size: bigint;
	};
}

/// Information obtained from the collator about authorship of a block.
interface IAuthorshipData {
	/// The header size in PoV in kb.
	header: number;
	/// The extrinsics size in PoV in kb.
	extrinsics: number;
	/// The storage proof size in PoV in kb.
	proof: number;
	/// The compressed PoV size (sum of all the above) in kb.
	compressed: number;
	/// The time it took to author the block in ms.
	time: number;
}

/// Print an event.
function pe(e: IEvent): string {
	return `${e.module} ${e.event} ${e.data ? safeJsonStringify(e.data) : "no data"}`;
}

interface IObservableEvent {
	chain: Chain;
	module: string;
	event: string;
	dataCheck: ((data: any) => boolean) | undefined;
	byBlock: number | undefined;
}

export class Observe {
	e: IObservableEvent;
	onPass: () => void = () => {};

	constructor(
		chain: Chain,
		module: string,
		event: string,
		dataCheck: ((data: any) => boolean) | undefined = undefined,
		byBlock: number | undefined = undefined,
		onPass: () => void = () => {}
	) {
		this.e = { chain, module, event, dataCheck, byBlock };
		this.onPass = onPass;
	}

	toString(): string {
		return `Observe(${this.e.chain}, ${this.e.module}, ${this.e.event}, ${
			this.e.dataCheck ? "dataCheck" : "no dataCheck"
		}, ${this.e.byBlock ? this.e.byBlock : "no byBlock"})`;
	}

	static on(chain: Chain, mod: string, event: string): ObserveBuilder {
		return new ObserveBuilder(chain, mod, event);
	}
}

export class ObserveBuilder {
	private chain: Chain;
	private module: string;
	private event: string;
	private dataCheck?: (data: any) => boolean;
	private byBlockVal?: number;
	private onPassCallback: () => void = () => {};

	constructor(chain: Chain, module: string, event: string) {
		this.chain = chain;
		this.module = module;
		this.event = event;
	}

	withDataCheck(check: (data: any) => boolean): ObserveBuilder {
		this.dataCheck = check;
		return this;
	}

	byBlock(blockNumber: number): ObserveBuilder {
		this.byBlockVal = blockNumber;
		return this;
	}

	onPass(callback: () => void): ObserveBuilder {
		this.onPassCallback = callback;
		return this;
	}

	build(): Observe {
		if (!this.module || !this.event) {
			throw new Error("Module and event are required");
		}

		return new Observe(
			this.chain,
			this.module,
			this.event,
			this.dataCheck,
			this.byBlockVal,
			this.onPassCallback
		);
	}
}

export enum EventOutcome {
	TimedOut = "TimedOut",
	Done = "Done",
}

export class TestCase {
	eventSequence: Observe[];
	onKill: () => void;
	allowPerChainInterleavedEvents: boolean = false;
	private resolveTestPromise: (outcome: EventOutcome) => void = () => {};

	/// See `example.test.ts` for more info.
	constructor(e: Observe[], interleave: boolean = false, onKill: () => void = () => {}) {
		this.eventSequence = e;
		this.onKill = onKill;
		this.allowPerChainInterleavedEvents = interleave;
	}

	setTestPromiseResolvers(resolve: (outcome: EventOutcome) => void) {
		this.resolveTestPromise = resolve;
	}

	match(ours: IObservableEvent, theirs: IEvent, theirsChain: Chain): boolean {
		const trivialComp =
			ours.chain === theirsChain &&
			ours.module === theirs.module &&
			ours.event === theirs.event;
		if (trivialComp) {
			// note: only run data check if it is defined and all other criteria match
			const dataComp = ours.dataCheck === undefined ? true : ours.dataCheck!(theirs.data);
			return trivialComp && dataComp;
		} else {
			return false;
		}
	}

	notTimedOut(ours: IObservableEvent, block: number): boolean {
		return ours.byBlock === undefined ? true : block <= ours.byBlock;
	}

	// with thousand separator!
	wts(num: bigint): string {
		return num.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
	}

	formatWeight(weight: IWeight): string {
		const weightPerMs = BigInt(Math.pow(10, 9));
		const WeightPerKb = BigInt(1024);
		const refTime =
			weight.normal.ref_time + weight.operational.ref_time + weight.mandatory.ref_time;
		const proofSize =
			weight.normal.proof_size + weight.operational.proof_size + weight.mandatory.proof_size;

		return `${this.wts(refTime / weightPerMs)}ms / ${this.wts(proofSize / WeightPerKb)} kb`;
	}

	formatAuthorship(authorship: IAuthorshipData): string {
		return `hd=${authorship.header.toFixed(2)}, xt=${authorship.extrinsics.toFixed(
			2
		)}, st=${authorship.proof.toFixed(2)}, sum=${(
			authorship.header +
			authorship.extrinsics +
			authorship.proof
		).toFixed(2)}, cmp=${authorship.compressed.toFixed(2)}, time=${authorship.time}ms`;
	}

	commonLog(blockData: IBlock): string {
		const number = `#${blockData.number}`;
		const chain = blockData.chain === Chain.Relay
			? chalk.blue(blockData.chain)     // Blue for Relay - works well in both modes
			: chalk.green(blockData.chain);   // Green for Parachain - works well in both modes
		const weight = `⛓ ${this.formatWeight(blockData.weights)}`;
		const authorship = blockData.authorship
			? `[✍️ ${this.formatAuthorship(blockData.authorship)}]`
			: "";
		return `[${chain}${number}][${weight}]${authorship}`;
	}

	// returns a [`primary`, `maybeSecondary`] event to check. `primary` should always be checked first, and if not secondary is checked.
	nextEvent(chain: Chain): [Observe, Observe | undefined] {
		const next = this.eventSequence[0]!;
		if (this.allowPerChainInterleavedEvents && this.eventSequence.length > 1) {
			// get the next event in our list that is of type `chain`.
			const nextOfChain = this.eventSequence.slice(1).find((e) => e.e.chain === chain);
			return [next, nextOfChain];
		} else {
			return [next, undefined];
		}
	}

	removeEvent(e: Observe): void {
		const index = this.eventSequence.findIndex((x) => x.e === e.e);
		if (index !== -1) {
			this.eventSequence.splice(index, 1);
		} else {
			logger.warn(`Event not found for removal: ${e.toString()}`);
			exit(1);
		}
	}

	onBlock(blockData: IBlock) {
		// sort from small to big
		logger.debug(`${this.commonLog(blockData)} events: ${blockData.events.length}`);
		const firstTimeOut = this.eventSequence
			.filter((e) => e.e.byBlock)
			.sort((x, y) => x.e.byBlock! - y.e.byBlock!);
		if (firstTimeOut.length > 0 && blockData.number > firstTimeOut[0]!.e.byBlock!) {
			logger.error(
				`Block ${blockData.number} is past the first timeout at block ${firstTimeOut[0]}, exiting.`
			);
			this.resolveTestPromise(EventOutcome.TimedOut);
		}

		for (const e of blockData.events) {
			this.onEvent(e, blockData);
		}
	}

	onEvent(e: IEvent, blockData: IBlock) {
		if (!this.eventSequence.length) {
			logger.warn(`No events to process for ${blockData.chain}, event: ${pe(e)}`);
			return;
		}
		logger.verbose(`${this.commonLog(blockData)} Processing event: ${pe(e)}`);
		const [primary, maybeSecondary] = this.nextEvent(blockData.chain);

		if (this.match(primary.e, e, blockData.chain)) {
			primary.onPass();
			this.removeEvent(primary);
			logger.info(`Primary event passed`);
			if (this.eventSequence.length === 0) {
				logger.info("All events processed.");
				this.resolveTestPromise(EventOutcome.Done);
			} else {
				logger.verbose(
					`Next expected event: ${this.eventSequence[0]!.toString()}, remaining events: ${
						this.eventSequence.length
					}`
				);
			}
		} else if (maybeSecondary && this.match(maybeSecondary.e, e, blockData.chain)) {
			maybeSecondary.onPass();
			this.removeEvent(maybeSecondary);
			logger.info(`Secondary event passed`);
			// when we check secondary events, we must have at least 2 items in the list, so no
			// need to check for the end of list.
		} else {
			logger.debug(`event not relevant`);
		}
	}
}

// Extract information about the authoring of `block` number from the given `logFile`. This will
// work in 3 steps:
// 1. After filtering for `[Parachain]`, and looking at the log file from end to start, it will find
//    the line containing `Prepared block for proposing at ${block}`. From this, we extract the
//    authoring time in ms
// 2. Them, we only keep the rest of the log file (optimization). We find the first line thereafter
//    containing `PoV size header_kb=... extrinsics_kb=... storage_proof_kb=...` and extract the
//    sizes of the header, extrinsics and storage proof.
// 3. Finally, we find the first line thereafter containing `Compressed PoV size: ...kb` and extract
//    the compressed size.
//
// Note: `logFile` must always relate to a parachain.
function extractAuthorshipData(block: number, logFile: string): IAuthorshipData | null {
	if (block == 0) {
		return null;
	}

	const log = readFileSync(logFile)
		.toString()
		.split("\n")
		.filter((l) => l.includes("[Parachain]"))
		.reverse();
	const target = `Prepared block for proposing at ${block}`;
	const findTime = (log: string[]): { time: number; readStack: string[] } => {
		const readStack: string[] = [];
		for (let i = 0; i < log.length; i++) {
			const line = log[i];
			if (!line) {
				continue;
			}
			readStack.push(line);
			if (line?.includes(target)) {
				const match = line.match("([0-9]+) ms");
				if (match) {
					return { time: Number(match.at(1)!), readStack };
				}
			}
		}
		throw `Could not find authorship line ${target}`;
	};

	const findProofs = (
		readStack: string[]
	): { header: number; extrinsics: number; proof: number } => {
		for (let i = 0; i < readStack.length; i++) {
			const line = readStack[i];
			const match = line?.match(
				"PoV size header_kb=([0-9]+.[0-9]+) extrinsics_kb=([0-9]+.[0-9]+) storage_proof_kb=([0-9]+.[0-9]+)"
			);
			if (match) {
				return {
					header: Number(match[1]!),
					extrinsics: Number(match[2]!),
					proof: Number(match[3])!,
				};
			}
		}
		throw "Could not find the expected PoV data in log file.";
	};

	const findCompressed = (readStack: string[]): number => {
		for (let i = 0; i < readStack.length; i++) {
			const line = readStack[i];
			const match = line?.match("Compressed PoV size: ([0-9]+.[0-9]+)kb");
			if (match) {
				return Number(match[1]!);
			}
		}
		throw "Could not find the expected compressed data in log file.";
	};

	const { time, readStack } = findTime(log);
	// reverse the read stack again, as we want the first proof related prints after we `findTime`.
	readStack.reverse();
	const { header, extrinsics, proof } = findProofs(readStack);
	const compressed = findCompressed(readStack);
	return { time, header, extrinsics, proof, compressed };
}

export async function runTest(
	test: TestCase,
	apis: ApiDeclarations,
	paraLog: string | null
): Promise<EventOutcome> {
	const { rcClient, paraClient, rcApi, paraApi } = apis;

	let completionPromise: Promise<EventOutcome> = new Promise((resolve, _) => {
		// Pass the resolve/reject functions to the TestCase instance
		test.setTestPromiseResolvers(resolve);

		rcClient.finalizedBlock$.subscribe(async (block) => {
			const events = await rcApi.query.System.Events.getValue({ at: block.hash });
			const weights = await rcApi.query.System.BlockWeight.getValue({ at: block.hash });
			const interested = events
				.filter(
					(e) =>
						e.event.type === "Session" ||
						e.event.type === "RootOffences" ||
						e.event.type === "StakingAhClient"
				)
				.map((e) => ({
					module: e.event.type,
					event: e.event.value.type,
					data: e.event.value.value,
				}));
			test.onBlock({
				chain: Chain.Relay,
				number: block.number,
				hash: block.hash,
				events: interested,
				weights: weights,
				authorship: null,
			});
		});

		paraClient.finalizedBlock$.subscribe(async (block) => {
			const events = await paraApi.query.System.Events.getValue({ at: block.hash });
			const weights = await paraApi.query.System.BlockWeight.getValue({ at: block.hash });
			const interested = events
				.filter(
					(e) =>
						e.event.type == "Staking" ||
						e.event.type == "MultiBlockElection" ||
						e.event.type == "MultiBlockElectionSigned" ||
						e.event.type == "MultiBlockElectionVerifier" ||
						e.event.type == "StakingRcClient" ||
						e.event.type == "Dap"
				)
				.map((e) => ({
					module: e.event.type,
					event: e.event.value.type,
					data: e.event.value.value,
				}));
			test.onBlock({
				chain: Chain.Parachain,
				number: block.number,
				hash: block.hash,
				events: interested,
				weights: weights,
				authorship: paraLog ? extractAuthorshipData(block.number, paraLog!) : null,
			});
		});
	});

	// Handle graceful exit on SIGINT
	process.on("SIGINT", () => {
		console.log("Exiting on Ctrl+C...");
		rcClient.destroy();
		paraClient.destroy();
		test.onKill();
		process.exit(0);
	});

	// Wait for the completionPromise to resolve/reject
	const finalOutcome = await completionPromise;
	rcClient.destroy();
	paraClient.destroy();
	logger.info(`Test completed with outcome: ${finalOutcome}, calling onKill...`);
	test.onKill();
	return finalOutcome;
}
