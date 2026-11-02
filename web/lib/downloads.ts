// Release asset names stay the same between versions, so these always point
// at the newest release.
const latest = "https://github.com/n1ckoates/elizabeats/releases/latest/download";

export const releaseNotes = "https://github.com/n1ckoates/elizabeats/releases/latest";

export type PlatformId = "mac-arm" | "mac-intel" | "windows" | "linux";

export interface Platform {
	id: PlatformId;
	name: string;
	button: string;
	files: { label: string; url: string }[];
}

export const platforms: Platform[] = [
	{
		id: "mac-arm",
		name: "macOS (Apple Silicon)",
		button: "Download for macOS",
		files: [
			{ label: "Installer (.pkg)", url: `${latest}/Elizabeats-macOS-AppleSilicon-Setup.pkg` },
			{ label: "Portable (.zip)", url: `${latest}/Elizabeats-macOS-AppleSilicon-Portable.zip` },
		],
	},
	{
		id: "mac-intel",
		name: "macOS (Intel)",
		button: "Download for macOS",
		files: [
			{ label: "Installer (.pkg)", url: `${latest}/Elizabeats-macOS-Intel-Setup.pkg` },
			{ label: "Portable (.zip)", url: `${latest}/Elizabeats-macOS-Intel-Portable.zip` },
		],
	},
	{
		id: "windows",
		name: "Windows",
		button: "Download for Windows",
		files: [
			{ label: "Installer (.exe)", url: `${latest}/Elizabeats-Windows-Setup.exe` },
			{ label: "Portable (.zip)", url: `${latest}/Elizabeats-Windows-Portable.zip` },
		],
	},
	{
		id: "linux",
		name: "Linux",
		button: "Download for Linux",
		files: [{ label: "AppImage", url: `${latest}/Elizabeats-Linux.AppImage` }],
	},
];

// Runs in the browser. Returns null for phones, tablets and anything else
// Elizabeats doesn't run on.
export async function detectPlatform(): Promise<Platform | null> {
	const ua = navigator.userAgent;
	let id: PlatformId | null = null;

	if (/Windows/.test(ua)) {
		id = "windows";
	} else if (/Macintosh/.test(ua) && navigator.maxTouchPoints < 2) {
		// iPads also claim to be a Mac, but have a touch screen.
		id = "mac-arm";
		// Only Chromium browsers can tell Intel from Apple Silicon. Everywhere
		// else, assume Apple Silicon, which every Mac since 2020 has.
		const uaData = (navigator as { userAgentData?: { getHighEntropyValues(hints: string[]): Promise<{ architecture?: string }> } }).userAgentData;
		try {
			const { architecture } = (await uaData?.getHighEntropyValues(["architecture"])) ?? {};
			if (architecture === "x86") id = "mac-intel";
		} catch {
			// Keep the Apple Silicon guess.
		}
	} else if (/Linux/.test(ua) && !/Android/.test(ua)) {
		id = "linux";
	}

	return platforms.find((platform) => platform.id === id) ?? null;
}
