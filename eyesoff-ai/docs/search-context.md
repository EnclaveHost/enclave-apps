# Search context and prefill cost

Search results contain new tokens. Prefix caching can reuse an unchanged system
prompt and conversation prefix, but it cannot skip reading fresh page content.
Previously, the default retrieval could feed three 6,000-character pages plus
snippets into every answer. Exa's synthesized snippet also repeated its page
opening. The cost appears after search completes, during model prefill.

`search.context_chars` now bounds the total source text rendered into a search
prompt. The default is 6,000 characters across all results; titles, URLs,
citation markers and safety instructions are additional. Short snippets retain
their full text and unused space is redistributed among longer sources.
`context_chars: 0` explicitly retains complete retrieved text. `page_chars`
continues to limit retrieval per page and is enforced locally on Exa responses.

Long source text is divided into bounded passages. Exact query-term overlap,
weighted toward terms that occur in fewer passages, selects relevant spans;
selected spans are restored to source order. This is deterministic extraction,
not generated summarization. Selected content remains verbatim, gaps are marked,
and partial results are labelled. All source titles, links and citation numbers
remain present. A snippet identical to the beginning of its page is not repeated;
a distinct snippet is included within that source's budget.

This trades exhaustive page coverage for lower initial prefill cost. Relevant
material can still be omitted by lexical selection, especially for synonyms or
queries poorly represented in the text. The prompt states that excerpts are
partial and directs the model to the existing `request` tool when more detail
is needed and that tool is available. Deployments requiring full retrieved text
can set the explicit zero limit. The ordinary page-fetch tool is unchanged.
Search results remain untrusted quoted data. No new provider, summary model,
credential scope, plaintext GPU path or isolation relaxation is introduced.

Tests cover late answer-bearing text, qualifiers and numbers, Unicode and tiny
budgets, deterministic ordering, source coverage, distinct snippets, duplicate
openings, full-context mode and source-text bounds. End-to-end comparisons must
use fixed provider results: live search can change result order and length,
which makes a simple before/after timing misleading. Local small-model timings
must not be reported as production 27B improvements.
