import { defineConfig } from "blume";
import { vercel } from "blume/deploy";

export default defineConfig({
	// Set explicitly so canonical URLs, og:url, the sitemap and JSON-LD always
	// use the custom domain, never a *.vercel.app one.
	deployment: vercel({ site: "https://elizabeats.nickoates.com" }),
	seo: {
		// Adds a SoftwareApplication node to the homepage's JSON-LD.
		software: {
			applicationCategory: "MultimediaApplication",
			operatingSystem: "macOS, Windows, Linux",
			license: "MIT",
			price: 0,
			sameAs: ["https://github.com/n1ckoates/elizabeats"],
		},
	},
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
		links: [{ label: "Made by Nick Oates", href: "https://nickoates.com" }],
	},
});
