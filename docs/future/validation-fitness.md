# Validation fitness for the evolution loop

`validate` returns Matched, DivergedAt, Differs or Trapped and runs optional SQL assertions; `promote_report` says whether receivers are affected. A self-evolving supervisor needs the loop closed: propose (LLM), validate on a fork with assertions, promote or reject, record rationale; memoized by content.

Done when: a supervisor behavior that receives a candidate message validates it and promotes only on Matched or Differs with all assertions true; a rejected candidate leaves a dead-letter style record naming the failing assertion.
