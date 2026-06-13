// Placeholder entry point — uPlot is bundled here for issue 9 (real-time charts).
// eslint-disable-next-line @typescript-eslint/no-unused-vars
import type {} from "uplot";

const app = document.querySelector<HTMLDivElement>("#app")!;
app.innerHTML = `
  <h1>HyfindTag Gateway</h1>
  <p>BLE central gateway — ready.</p>
`;
