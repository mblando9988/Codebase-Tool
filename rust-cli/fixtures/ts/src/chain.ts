import { log } from "./util/log";

export function stepOne(): number {
  return stepTwo() + 1;
}
export function stepTwo(): number {
  return stepThree() + 1;
}
export function stepThree(): number {
  return stepFour() + 1;
}
export function stepFour(): number {
  log("four");
  return stepFive() + 1;
}
export function stepFive(): number {
  return 5;
}
