# tiny-eye documentation site

[Docusaurus 3](https://docusaurus.io) site for tiny-eye. The content is the Markdown in
[`../docs`](../docs), served under `/docs`; this folder holds the site configuration, the theme
and the landing page ([`src/pages/index.tsx`](src/pages/index.tsx)). `../docs/assets` is also a
static directory, so its screenshots and video are served from the site root.

Requires Node.js 20+.

```bash
cd website
npm install

# live preview on http://localhost:3000/ (reloads on edits in ../docs)
BASE_URL=/ npm start

# production build into website/build (fails on broken links), then serve it
BASE_URL=/ npm run build
BASE_URL=/ npm run serve

npm run clear   # drop the .docusaurus cache if something looks stale
```

Without `BASE_URL`, the site is built for GitHub Pages at `/<repo-name>/`; the owner and repo
name come from `GITHUB_REPOSITORY` (set by GitHub Actions). Deployment is
[`.github/workflows/pages.yml`](../.github/workflows/pages.yml), on every push to `main` that
touches `docs/` or `website/` (enable Pages with source "GitHub Actions" in the repo settings).

Sidebar: hand-written in [`sidebars.ts`](sidebars.ts), the one place that orders and labels the
docs (groups: Get started, Guides, Concepts, Reference). A new page must be added there, except
reports under `docs/deep-dive/evidence/`, which are listed automatically. Search is local
(`@easyops-cn/docusaurus-search-local`); the index is built by `npm run build`, so it does not
work in `npm start`.
