import { Chart, Table, exportedFn, arrowFn } from "./shapes";
import { log } from "./util/log";

export function draw(): string {
  const c = new Chart();
  const t = new Table();
  log("draw");
  return c.render() + t.render() + exportedFn(1) + arrowFn(2);
}

export function other(): number {
  return exportedFn(3);
}
