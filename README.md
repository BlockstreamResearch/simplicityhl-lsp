# SimplicityHL LSP

This project was originally part of [SimplicityHL](https://github.com/BlockstreamResearch/SimplicityHL), the high-level language for writing Simplicity smart contracts.

## Overview

This repository contains:

- [lsp/](lsp/) — A Language Server Protocol implementation for SimplicityHL, providing diagnostics, completions, hover, and go-to-definition support.
- [vscode/](vscode/) — A VSCode extension that provides syntax highlighting and integrates with the LSP.

## What is SimplicityHL?

[Simplicity](https://github.com/BlockstreamResearch/simplicity) is a typed, combinator-based, functional language without loops or recursion, developed as an alternative to Bitcoin Script that is formally specified and can be statically analyzed.

SimplicityHL is a high-level language for writing Simplicity smart contracts. It looks and feels like [Rust](https://www.rust-lang.org), but compiles to Simplicity bytecode. Developers write SimplicityHL transactions, which Bitcoin/Liquid nodes verify with the Simplicity script interpreter.

## Getting Started

See the individual READMEs for setup and usage:

- [LSP README](lsp/README.md)
- [VSCode Extension README](vscode/README.md)
