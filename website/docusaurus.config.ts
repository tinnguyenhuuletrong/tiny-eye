import type * as Preset from "@docusaurus/preset-classic";
import type { Config } from "@docusaurus/types";
import { themes as prismThemes } from "prism-react-renderer";

// Derived from the environment so the site builds for any fork on GitHub Pages.
// GITHUB_REPOSITORY is set by GitHub Actions ("owner/name"). Locally: BASE_URL=/ npm start
const repo = process.env.GITHUB_REPOSITORY ?? "OWNER/tiny-eye";
const [owner, name] = repo.split("/");
const repoUrl = `https://github.com/${repo}`;

const config: Config = {
  title: "tiny-eye",
  tagline: "Blob-first observability: metrics, logs and traces in Parquet, queried with TQS",
  favicon: "img/favicon.svg",

  url: `https://${owner}.github.io`,
  baseUrl: process.env.BASE_URL ?? `/${name}/`,
  organizationName: owner,
  projectName: name,
  trailingSlash: false,

  onBrokenLinks: "throw",
  markdown: {
    format: "detect",
    mermaid: true,
    hooks: {
      onBrokenMarkdownLinks: "throw",
    },
  },
  themes: ["@docusaurus/theme-mermaid"],

  i18n: { defaultLocale: "en", locales: ["en"] },

  presets: [
    [
      "classic",
      {
        // The docs are the whole site: no blog, no separate landing page (docs/intro.md has slug: /).
        docs: {
          path: "../docs",
          routeBasePath: "/",
          sidebarPath: "./sidebars.ts",
          editUrl: ({ docPath }) => `${repoUrl}/tree/main/docs/${docPath}`,
          showLastUpdateTime: false,
        },
        blog: false,
        theme: { customCss: "./src/css/custom.css" },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    colorMode: { respectPrefersColorScheme: true },
    navbar: {
      title: "tiny-eye",
      logo: { alt: "tiny-eye", src: "img/logo.svg" },
      items: [
        { to: "/category/getting-started", label: "Getting started", position: "left" },
        { to: "/category/examples", label: "Examples", position: "left" },
        { to: "/category/deep-dive", label: "Deep dive", position: "left" },
        { to: "/category/reference", label: "Reference", position: "left" },
        { href: repoUrl, label: "GitHub", position: "right" },
      ],
    },
    footer: {
      style: "light",
      links: [
        {
          title: "Docs",
          items: [
            { label: "Getting started", to: "/category/getting-started" },
            { label: "Examples", to: "/category/examples" },
            { label: "Deep dive", to: "/category/deep-dive" },
            { label: "Reference", to: "/category/reference" },
          ],
        },
        {
          title: "Project",
          items: [
            { label: "GitHub", href: repoUrl },
            { label: "Issues", href: `${repoUrl}/issues` },
            { label: "Contributing", href: `${repoUrl}/blob/main/CONTRIBUTING.md` },
            { label: "Changelog", href: `${repoUrl}/blob/main/CHANGELOG.md` },
          ],
        },
      ],
      copyright: `Copyright ${new Date().getFullYear()} tiny-eye contributors. Apache-2.0.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      additionalLanguages: ["rust", "bash", "json", "yaml", "toml"],
    },
    mermaid: {
      theme: { light: "neutral", dark: "dark" },
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
