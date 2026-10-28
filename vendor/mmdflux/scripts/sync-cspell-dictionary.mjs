#!/usr/bin/env node

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const sourcePath = resolve(repoRoot, "codebook.toml");
const outputPath = resolve(repoRoot, ".cspell", "codebook.txt");
const checkOnly = process.argv.includes("--check");

const supportedArgs = new Set(["--check"]);
for (const arg of process.argv.slice(2)) {
  if (!supportedArgs.has(arg)) {
    throw new Error(`unsupported argument: ${arg}`);
  }
}

const source = readFileSync(sourcePath, "utf8");
const words = parseCodebookWords(source);
const generated = [
  "# Generated from ../codebook.toml by scripts/sync-cspell-dictionary.mjs.",
  "# Do not edit directly; update codebook.toml and run `just spell-sync`.",
  "",
  ...words,
  "",
].join("\n");

if (checkOnly) {
  if (
    !existsSync(outputPath) ||
    readFileSync(outputPath, "utf8") !== generated
  ) {
    console.error(".cspell/codebook.txt is out of sync with codebook.toml");
    console.error("run `just spell-sync` to regenerate it");
    process.exit(1);
  }
  process.exit(0);
}

mkdirSync(dirname(outputPath), { recursive: true });
writeFileSync(outputPath, generated);

function parseCodebookWords(source) {
  const assignment = /^words\s*=\s*\[/m.exec(source);
  if (!assignment) {
    throw new Error("expected a top-level `words = [` array in codebook.toml");
  }

  const words = [];
  let index = assignment.index + assignment[0].length;

  while (index < source.length) {
    index = skipTrivia(source, index);

    if (source[index] === "]") {
      return validateWords(words);
    }

    const quote = source[index];
    if (quote !== '"' && quote !== "'") {
      throw parseError(
        source,
        index,
        "expected a TOML string or closing bracket",
      );
    }

    const parsed = readTomlString(source, index);
    words.push(parsed.value);
    index = skipTrivia(source, parsed.next);

    if (source[index] === ",") {
      index++;
      continue;
    }

    if (source[index] === "]") {
      return validateWords(words);
    }

    throw parseError(source, index, "expected a comma or closing bracket");
  }

  throw new Error("unterminated `words` array in codebook.toml");
}

function skipTrivia(source, index) {
  while (index < source.length) {
    const char = source[index];
    if (/\s/.test(char)) {
      index++;
      continue;
    }
    if (char === "#") {
      while (index < source.length && source[index] !== "\n") index++;
      continue;
    }
    break;
  }
  return index;
}

function readTomlString(source, index) {
  if (source[index] === "'") {
    return readLiteralString(source, index);
  }
  return readBasicString(source, index);
}

function readLiteralString(source, index) {
  let value = "";
  for (let i = index + 1; i < source.length; i++) {
    const char = source[i];
    if (char === "'") {
      return { value, next: i + 1 };
    }
    value += char;
  }
  throw parseError(source, index, "unterminated TOML literal string");
}

function readBasicString(source, index) {
  let json = '"';
  for (let i = index + 1; i < source.length; i++) {
    const char = source[i];
    json += char;

    if (char === "\\") {
      i++;
      if (i >= source.length) break;
      json += source[i];
      continue;
    }

    if (char === '"') {
      try {
        return { value: JSON.parse(json), next: i + 1 };
      } catch (error) {
        throw parseError(
          source,
          index,
          `invalid TOML basic string: ${error.message}`,
        );
      }
    }
  }
  throw parseError(source, index, "unterminated TOML basic string");
}

function validateWords(words) {
  if (words.length === 0) {
    throw new Error("codebook.toml words array is empty");
  }

  const seen = new Set();
  for (const word of words) {
    if (word.length === 0) {
      throw new Error("codebook.toml contains an empty dictionary word");
    }
    if (/\s/.test(word)) {
      throw new Error(
        `cspell dictionary entries cannot contain whitespace: ${word}`,
      );
    }
    if (seen.has(word)) {
      throw new Error(`duplicate codebook dictionary word: ${word}`);
    }
    seen.add(word);
  }
  return words;
}

function parseError(source, index, message) {
  const prefix = source.slice(0, index);
  const line = prefix.split("\n").length;
  const column = prefix.length - prefix.lastIndexOf("\n");
  return new Error(`${message} at codebook.toml:${line}:${column}`);
}
