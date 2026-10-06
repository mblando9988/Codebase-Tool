export function isEven(n: number): boolean {
  return n === 0 ? true : isOdd(n - 1);
}
export function isOdd(n: number): boolean {
  return n === 0 ? false : isEven(n - 1);
}
export function factorial(n: number): number {
  return n <= 1 ? 1 : n * factorial(n - 1);
}
