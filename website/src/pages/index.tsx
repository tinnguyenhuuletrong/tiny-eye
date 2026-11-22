import Link from "@docusaurus/Link";
import useBaseUrl from "@docusaurus/useBaseUrl";
import useDocusaurusContext from "@docusaurus/useDocusaurusContext";
import Layout from "@theme/Layout";
import { type ReactNode, useState } from "react";
import styles from "./index.module.css";

// Every claim on this page is backed by the docs it links to; numbers come from docs/deep-dive/evidence.
const QUICKSTART = "/docs/getting-started/quickstart";
const MCP_GUIDE = "/docs/examples/ai-agents-mcp";
const MCP_ADD = "claude mcp add tiny-eye -- tiny-eye mcp --url http://127.0.0.1:7070";

const WHY = "/docs/getting-started/why-tiny-eye";
// "What makes it different": built items only, plus planned ones labelled as such (see the docs page).
const DIFFERENT: { title: string; text: string; anchor: string; planned?: boolean }[] = [
  {
    title: "Files that outlive the engine",
    text: "Plain Parquet and a DuckLake catalog in your bucket. Any Parquet reader opens them.",
    anchor: "your-telemetry-is-open-parquet-you-own",
  },
  {
    title: "Queryable once acknowledged",
    text: "Queries read the lake and the in-memory hot tail together, each row counted once.",
    anchor: "acknowledged-means-queryable",
  },
  {
    title: "Rate panels in one pass",
    text: "Running sums and ASOF joins: a 6 h, 2,000-series rate panel went from 3.7 s to 0.58 s.",
    anchor: "panel-cost-grows-with-series-and-steps-not-window-size",
  },
  {
    title: "Maintenance scoped to new data",
    text: "Checkpoints apply only new flushes; compaction rewrites only hours with new files.",
    anchor: "maintenance-only-rewrites-what-changed",
  },
  {
    title: "Materialized panels",
    text: "Mark a panel materialize: true; long-range reads use stored points, 10–12× faster, same answer.",
    anchor: "materialized-panels-maintained-incrementally",
  },
];

const PROOF = [
  { title: "Prometheus-exact", text: "105/105 conformance cases", to: "/docs/deep-dive/evidence/tqs-coverage" },
  { title: "Parquet you own", text: "on local disk or S3", to: "/docs/deep-dive/storage-format" },
  { title: "One binary", text: "UI, API, MCP built in", to: "/docs/deep-dive/architecture" },
  { title: "Query as data", text: "TQS JSON, validated", to: "/docs/deep-dive/tqs-reference" },
];

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () =>
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  return (
    <button type="button" className={styles.copy} onClick={copy} aria-label="Copy to clipboard">
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

function Feature(props: { eyebrow: string; title: string; to: string; link: string; children: ReactNode; media: ReactNode; flip?: boolean }) {
  return (
    <section className={`${styles.feature} ${props.flip ? styles.flip : ""}`}>
      <div className={styles.featureText}>
        <p className={styles.eyebrow}>{props.eyebrow}</p>
        <h2>{props.title}</h2>
        <p>{props.children}</p>
        <Link to={props.to}>{props.link} →</Link>
      </div>
      <div className={styles.featureMedia}>{props.media}</div>
    </section>
  );
}

export default function Home(): ReactNode {
  const { organizationName, projectName, tagline } = useDocusaurusContext().siteConfig;
  const repoUrl = `https://github.com/${organizationName}/${projectName}`;
  const tryIt = `git clone ${repoUrl} && cd ${projectName} && make demo`;
  return (
    <Layout title="Observability in one binary" description={tagline}>
      <main className={styles.page}>
        <header className={styles.hero}>
          <div className={styles.heroText}>
            <h1>Metrics, logs and traces in one binary, stored as Parquet you own.</h1>
            <p className={styles.lead}>
              tiny-eye ingests Prometheus remote-write, OTLP and Loki push, writes sorted ZSTD Parquet to disk or S3, and answers
              TQS queries whose metric results match Prometheus point for point.
            </p>
            <div className={styles.buttons}>
              <Link className="button button--primary button--lg" to={QUICKSTART}>
                Get started
              </Link>
              <Link className="button button--secondary button--outline button--lg" to={MCP_GUIDE}>
                Connect an AI agent
              </Link>
            </div>
          </div>
          <video
            className={`screenshot ${styles.heroMedia}`}
            src={useBaseUrl("/how-it-works.mp4")}
            poster={useBaseUrl("/how-it-works.png")}
            autoPlay
            muted
            loop
            playsInline
            aria-label="How tiny-eye works: ingest, Parquet, DuckLake, TQS"
          />
        </header>

        <ul className={styles.proof}>
          {PROOF.map((p) => (
            <li key={p.title}>
              <Link to={p.to}>
                <strong>{p.title}</strong>
                <span>{p.text}</span>
              </Link>
            </li>
          ))}
        </ul>

        <section className={styles.different}>
          <div className={styles.differentHead}>
            <h2>What makes it different</h2>
            <Link to={WHY}>Read the details and the evidence →</Link>
          </div>
          <ul>
            {DIFFERENT.map((d) => (
              <li key={d.title}>
                <Link to={`${WHY}#${d.anchor}`}>
                  <strong>
                    {d.title}
                    {d.planned && <span className={styles.planned}>Planned</span>}
                  </strong>
                  <span>{d.text}</span>
                </Link>
              </li>
            ))}
          </ul>
        </section>

        <Feature
          eyebrow="Dashboards"
          title="Dashboards that live in git"
          to="/docs/examples/dashboards-as-code"
          link="Build dashboards as code"
          media={<img className="screenshot" src={useBaseUrl("/ui-dashboard.jpg")} alt="The Shop overview dashboard" loading="lazy" />}
        >
          Each dashboard is a YAML file of TQS panels. Drag and resize panels in the web UI and the layout is saved back to the file.
        </Feature>

        <Feature
          flip
          eyebrow="Explore and traces"
          title="From a spike to the span"
          to="/docs/examples/query-cookbook"
          link="Browse the query cookbook"
          media={<img className="screenshot" src={useBaseUrl("/ui-explore.jpg")} alt="Explore with the Builder and TQS switch" loading="lazy" />}
        >
          Build a query with the Builder or write TQS directly, open any trace as a waterfall, and group log lines into patterns.
        </Feature>

        <Feature
          eyebrow="AI agents (MCP)"
          title="Let an agent query your telemetry"
          to={MCP_GUIDE}
          link="Connect an AI agent"
          media={
            <div className={styles.terminal}>
              <div className={styles.terminalBar}>
                <span />
                <span />
                <span />
                <CopyButton text={MCP_ADD} />
              </div>
              <pre>
                <code>
                  <span className={styles.prompt}>$ </span>
                  {MCP_ADD}
                  {"\n\n"}
                  <span className={styles.comment}># read-only tools the agent gets{"\n"}</span>
                  {"discover_telemetry_catalog\nvalidate_tqs\nrun_tqs"}
                </code>
              </pre>
            </div>
          }
        >
          <code>tiny-eye mcp</code> is a stdio MCP bridge. The schema is closed, so an invented field is reported back to the agent
          instead of being ignored.
        </Feature>

        <section className={styles.cta}>
          <div>
            <h2>Run it on your laptop</h2>
            <p>Needs Rust, a C/C++ compiler and Node.js 20+. The first build compiles DuckDB; then the web UI is on http://127.0.0.1:7070.</p>
          </div>
          <div className={styles.ctaCode}>
            <code>{tryIt}</code>
            <CopyButton text={tryIt} />
          </div>
          <nav className={styles.ctaLinks}>
            <Link to="/docs/intro">Docs</Link>
            <Link href={repoUrl}>GitHub</Link>
            <Link to="/docs/category/guides">Examples</Link>
          </nav>
        </section>
      </main>
    </Layout>
  );
}
