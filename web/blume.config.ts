import { defineConfig } from "blume";

export default defineConfig({
	title: "Elizabeats",
	description:
		"Simple radio playout for DJs. Search your local music folder and YouTube Music, queue up tracks and talk breaks, and Elizabeats plays it all while you’re on the mic.",
	logo: "/icon.png",
	basePath: "/docs",
	github: { owner: "n1ckoates", repo: "elizabeats" },
	navigation: {
		// basePath turns "/" into "/docs".
		actions: [{ href: "/", label: "Docs" }],
	},
	theme: {
		accent: "red",
	},
	footer: {
		links: [{ label: "Made by Nick Oates", href: "https://nickoates.com" }],
	},
});
