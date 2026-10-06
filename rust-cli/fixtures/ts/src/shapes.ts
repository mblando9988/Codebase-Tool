import { log } from "./util/log";

/** Anything that can draw itself as text. */
export interface Renderable {
  render(): string;
}

/**
 * A chart. The documentation here is deliberately long so that anything which clips long
 * documentation has something to clip: it keeps going well past the limit that the server
 * applies to a single text field, and it repeats itself to be sure of that. It keeps going
 * well past the limit that the server applies to a single text field, and it repeats itself
 * to be sure of that.
 */
export class Chart implements Renderable {
  title = "t";
  render(): string {
    log("chart");
    return "chart";
  }
}

export class Table implements Renderable {
  render(): string {
    log("table");
    return "table";
  }
}

export function exportedFn(a: number): number {
  return a + 1;
}
export const arrowFn = (x: number): number => x * 2;
function internalFn(a: number): number {
  return a > 1 ? a : 0;
}
export enum Color {
  Red,
  Green,
}
export type Id = string;
export function usesInternal(): number {
  return internalFn(5);
}
export function veryLongSignature(alpha: number, beta: string, gamma: boolean, delta: Id, epsilon: Color, zeta: Renderable[], eta: Map<string, number>): Promise<Map<string, Renderable[]>> {
  return Promise.resolve(new Map());
}
export interface Pair { left: number;
  right: number }
