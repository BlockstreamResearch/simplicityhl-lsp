# SimplicityHL extension for VSCode

VSCode extension that provides syntax highlighting for the [SimplicityHL](https://github.com/BlockstreamResearch/SimplicityHL) programming language.

[Simplicity](https://github.com/BlockstreamResearch/simplicity) is a typed, combinator-based, functional language without loops or recursion, developed to be an alternative to Bitcoin script that is formally specified, and can be statically analyzed with upper bounds on computation resources prior to execution.

SimplicityHL is a high-level language for writing Simplicity smart contracts. SimplicityHL looks and feels like [Rust](https://www.rust-lang.org), but is compiled to Simplicity bytecode. Developers write SimplicityHL transactions, which Bitcoin/Liquid nodes verify with the Simplicity script interpreter.

## Features

- Syntax highlighting for .simf and .wit files
- Basic language configuration (brackets, comments)

Also, you can install the [SimplicityHL language server](https://github.com/distributed-lab/simplicityhl-lsp), which enables several features:
- Error diagnostics
![diagnostics](https://github.com/user-attachments/assets/54315645-464b-40c3-bb72-c6e8c4bc0ad5)

- Completion of user-defined functions and jets
![completion](https://github.com/user-attachments/assets/bbc2b9de-c286-4d31-b47e-ac95885f8916)




### Development

To install the extension manually or hack on the source code see [development.md](docs/development.md)
