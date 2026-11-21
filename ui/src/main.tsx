import "@fontsource-variable/geist";
import "@fontsource-variable/geist-mono";
import "uplot/dist/uPlot.min.css";
import "./styles.css";
import { render } from "preact";
import { App } from "./app";

render(<App />, document.getElementById("app")!);
