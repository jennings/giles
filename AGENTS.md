# Agent instructions

- Docs/IDEA.md contains a the general idea for this project
- Docs/PLAN.md contains design notes for this project
- TASKS.md contains an implementation plan

## Build

- Cargo is the repository build system.
- Create documentation with mdBook in the Docs directory
  - User documentation describes what the system is, how to use it, how to
    configure it, etc.
  - Technical documentation describes significant design decisions of the
    implementation: Architecture overview, wire protocols, major subsystems,
    etc.

## Development process

- Update tests in the same commit that changes behavior.
- Update documentation in the same commit when the system changed:
  - Update user documentation for any user-facing change.
  - Update technical documentation for implementation changes.

## Commits

- Every commit must build and pass all tests.
- Every commit must be properly formatted.
- Each commit should contain an atomic unit of work.
- Do not mix refactoring and functionality changes in a single commit. First
  refactor, then change functionality.
- Commits should be sized to be human reviewable, whenever reasonably possible.
  (Exception: Generated files, data)
- Commit messages should follow the Conventional Commits style.
