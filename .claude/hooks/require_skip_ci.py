#!/usr/bin/env python3
"""Keep GitHub Actions from running.

GitHub starts no Actions run for a push or pull request whose head commit message holds
a skip tag. This Claude Code PreToolUse hook refuses the calls that would leave a commit,
a pull request title or a merge message without one. Exit code 2 refuses the call and
shows stderr to Claude; any other failure lets the call through.

Usage: require_skip_ci.py bash | message | title | commit_title
"""
import json
import os
import re
import subprocess
import sys

TAG = re.compile(r"\[(?:skip ci|ci skip|no ci|skip actions|actions skip)\]")

# A git command at the start of a shell command or after && || ; | ( or a newline, so text
# that only mentions "git push" (an echo, a grep, a commit message) is not mistaken for one.
GIT = r"(?:^|[;&|(\n])\s*(?:sudo\s+)?git(?:\s+-[Cc]\s+\S+|\s+--[\w-]+(?:=\S+)?)*\s+"
COMMIT = re.compile(GIT + r"commit(?=\s|$)")
PUSH = re.compile(GIT + r"push(?=\s|$)")
CD = re.compile(r"(?:^|[;&|(\n])\s*cd\s+(\"[^\"]*\"|'[^']*'|\S+)")
C_OPTION = re.compile(r"\s-C\s+(\"[^\"]*\"|'[^']*'|\S+)")
DELETE = re.compile(r"\s(?:--delete|-d)\s")

COMMIT_HELP = (
    "Refused: this commit message has no skip tag. A pushed commit without one starts a "
    "GitHub Actions run, and this account's Actions are billing-locked, so it would only fail.\n"
    'Put [skip ci] in the message, e.g. git commit -m "Short summary [skip ci]". It has to be '
    "written in the command itself (-m or a heredoc), not read from a file."
)
PUSH_HELP = (
    "Refused: the commit at the head of this push has no skip tag, so GitHub would start an "
    "Actions run.\n"
    'Amend it first: git commit --amend -m "<subject> [skip ci]" (repeat the trailers). '
    "Only amend a commit that has not been pushed."
)
FIELD_HELP = {
    "message": "Refused: this commit message has no skip tag, so the push would start a GitHub "
    "Actions run. Add [skip ci] to the message.",
    "title": "Refused: the pull request title has no skip tag. GitHub puts the title in the "
    "merge commit, so without [skip ci] there, merging starts an Actions run. Add [skip ci] "
    "to the end of the title.",
    "commit_title": "Refused: the merge commit message has no skip tag, so the merge would start "
    "a GitHub Actions run. Add [skip ci] to the commit title.",
}


def refuse(text):
    print(text, file=sys.stderr)
    sys.exit(2)


def check_field(field, arguments):
    names = ("commit_title", "commit_message") if field == "commit_title" else (field,)
    given = [arguments[n] for n in names if isinstance(arguments.get(n), str)]
    if given and not any(TAG.search(text) for text in given):
        refuse(FIELD_HELP[field])


def push_folder(command, push, cwd):
    folder = cwd
    for found in CD.finditer(command[: push.start()]):
        folder = os.path.join(folder, os.path.expandvars(os.path.expanduser(found.group(1).strip("\"'"))))
    option = C_OPTION.search(push.group(0))
    if option:
        folder = os.path.join(folder, os.path.expandvars(os.path.expanduser(option.group(1).strip("\"'"))))
    return folder


def check_bash(command, cwd):
    if COMMIT.search(command):
        # The message is being written by this very command, so it has to show the tag.
        if not TAG.search(command):
            refuse(COMMIT_HELP)
        return
    push = PUSH.search(command)
    if not push or DELETE.search(command):
        return
    try:
        head = subprocess.run(
            ["git", "-C", push_folder(command, push, cwd), "log", "-1", "--format=%B"],
            capture_output=True, text=True, timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired):
        return
    if head.returncode == 0 and not TAG.search(head.stdout):
        refuse(PUSH_HELP)


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "bash"
    data = json.load(sys.stdin)
    arguments = data.get("tool_input") or {}
    if mode == "bash":
        check_bash(arguments.get("command") or "", data.get("cwd") or os.getcwd())
    else:
        check_field(mode, arguments)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # a broken hook must not stop work, but it must not be silent
        print(f"require_skip_ci.py could not run: {error}", file=sys.stderr)
        sys.exit(1)
