# Contributing

Please read and follow [AGENTS.md](AGENTS.md) before contributing.

## AI review and testing

**All code contributions must undergo AI-assisted review and audit.** The coding
agent must first read the relevant repository code, check the applicable requirements,
and confirm the problem being addressed. Its review must assess the proposed solution
against that context.

Before submitting a pull request:

- Use AI to review the change in context for correctness, security, failure handling
  and regressions. Address the findings and review the final revision.
- Use AI to identify relevant test scenarios and gaps in coverage. Run meaningful tests
  for the affected behavior and the applicable checks in the [CI workflow](.github/workflows/ci.yml).
- Report failed or unavailable checks and anything that remains unverified. Do not
  bypass checks or weaken tests to obtain a passing result.

An AI assessment does not replace executed tests or maintainer review. Contributors
remain responsible for understanding their changes and the evidence supporting them.

## Pull requests

Keep each pull request focused. Include:

- What changed and why.
- The AI tools used, the scope of their review and audit, and how findings were addressed.
- The verification commands run, their results, and any checks not run or known limitations.

A concise summary is sufficient; do not include private transcripts, credentials or
machine details. Code contributions without the required AI review and verification
summary will not be accepted.

## Community discussion

We welcome discussion and contributions from mathematicians, researchers and developers.
Put Open Math Harness to work, share what you learn and help shape what comes next.

- **Discuss** ideas, priorities and results in the
  [Open-Math-Model channel](https://zulip.sair.foundation/#narrow/channel/27-Open-Math-Model)
  on the SAIR Foundation Zulip. It is a good place to talk through a larger change before
  you open a pull request.
- **Share** feedback, bug reports and suggestions from your own research and use.
- **Contribute** code and documentation through pull requests that meet the requirements
  above.
