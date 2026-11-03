import { defineConfig } from "blume";
import { vercel } from "blume/deploy";

export default defineConfig({
	deployment: vercel({
		imageService: true,
		imagesConfig: {
			sizes: [88, 176, 250, 500, 640, 828, 1080, 1200, 1920, 2000],
			formats: ["image/avif", "image/webp"],
			domains: [],
		},
	}),
	title: "Elizabeats",
	description:
		"Simple radio playout for DJs. Search your local music folder and YouTube Music, queue up tracks and talk breaks, and Elizabeats plays it all while you’re on the mic.",
	logo: "/icon.png",
	basePath: "/docs",
	github: { owner: "n1ckoates", repo: "elizabeats" },
	theme: {
		accent: "red",
	},
	footer: {
		links: [
			{ label: "Made by Nick Oates", href: "https://nickoates.com" },
			{ label: "Privacy", href: "/privacy" },
		],
	},
});
