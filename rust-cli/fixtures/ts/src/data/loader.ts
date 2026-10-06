import { connect } from "../db/connection";
import { log } from "../util/log";

export function load(): null {
  log("load");
  return connect();
}
