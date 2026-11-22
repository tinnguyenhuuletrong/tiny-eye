import type * as Preset from "@docusaurus/preset-classic";
import type { Config } from "@docusaurus/types";
import { themes as prismThemes } from "prism-react-renderer";

// Derived from the environment so the site builds for any fork on GitHub Pages.
// GITHUB_REPOSITORY is set by GitHub Actions ("owner/name"). Locally: BASE_URL=/ npm start
const repo = process.env.GITHUB_REPOSITORY ?? "tinnguyenhuuletrong/tiny-eye";
const [owner, name] = repo.split("/");
const repoUrl = `https://github.com/${repo}`;
const baseUrl = process.env.BASE_URL ?? `/${name}/`;

const config: Config = {
  title: "tiny-eye",
  tagline: "Blob-first observability: metrics, logs and traces in Parquet, queried with TQS",
  favicon: "img/favicon.svg",

  url: `https://${owner}.github.io`,
  baseUrl,
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
  // docs/assets holds the screenshots and the how-it-works video: the docs embed them relatively,
  // the landing page serves them from the site root.
  staticDirectories: ["static", "../docs/assets"],
  themes: [
    "@docusaurus/theme-mermaid",
    [
      "@easyops-cn/docusaurus-search-local",
      { hashed: true, docsDir: "../docs", docsRouteBasePath: "/docs", indexBlog: false },
    ],
  ],

  i18n: { defaultLocale: "en", locales: ["en"] },

  presets: [
    [
      "classic",
      {
        // Landing page: src/pages/index.tsx. Docs live under /docs, no blog.
        docs: {
          path: "../docs",
          routeBasePath: "/docs",
          sidebarPath: "./sidebars.ts",
          editUrl: ({ docPath }) => `${repoUrl}/tree/main/docs/${docPath}`,
          showLastUpdateTime: true,
        },
        blog: false,
        theme: { customCss: "./src/css/custom.css" },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    image: "ui-dashboard.jpg",
    colorMode: { respectPrefersColorScheme: true },
    announcementBar: {
      id: "pre-release",
      content: `tiny-eye is pre-release: single node, APIs may change. <a href="${baseUrl}docs/deep-dive/limitations-and-roadmap">Limitations</a>`,
      backgroundColor: "var(--te-surface)",
      textColor: "var(--ifm-font-color-base)",
      isCloseable: true,
    },
    navbar: {
      title: "tiny-eye",
      logo: { alt: "tiny-eye", src: "img/logo.svg" },
      items: [
        { type: "doc", docId: "intro", label: "Docs", position: "left" },
        { to: "/docs/category/guides", label: "Guides", position: "left" },
        { to: "/docs/category/concepts", label: "Concepts", position: "left" },
        { to: "/docs/category/reference", label: "Reference", position: "left" },
        { href: repoUrl, position: "right", className: "navbar-github", "aria-label": "GitHub repository" },
      ],
    },
    footer: {
      style: "light",
      links: [
        {
          title: "Docs",
          items: [
            { label: "Get started", to: "/docs/category/get-started" },
            { label: "Guides", to: "/docs/category/guides" },
            { label: "Concepts", to: "/docs/category/concepts" },
            { label: "Reference", to: "/docs/category/reference" },
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
      darkTheme: prismThemes.vsDark,
      additionalLanguages: ["rust", "bash", "json", "yaml", "toml"],
    },
    mermaid: {
      theme: { light: "neutral", dark: "dark" },
    },
  } satisfies Preset.ThemeConfig,
};

export default config;
