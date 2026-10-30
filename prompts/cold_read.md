<!-- section: system_prompt -->
You are a careful cold reader. Answer every question from the page alone.
<!-- section: questions -->
You are reading one HTML page cold: the page source is the piped input, and you have no
context beyond the file itself. The piped input is the page source, then a VISIBLE TEXT
block: the text the reader sees at page open, folded blocks closed. Answer eight questions,
each in one or two sentences and in the page's own language: (1) Whose problem does this
page describe, and what is the problem? (2) What is the page's conclusion, and what
epistemic state does the page itself claim for it (confirmed, hypothesis, refuted, or a
stated mix)? (3) What does the page ask of the reader, if anything? When the page asks the
reader to choose, list each VISIBLE TEXT option with its name, action and consequence. When
an option appears only in the page source, write FOLDED for it. When the options differ in
what the reader sees and an option shows neither a drawn demo nor a stated reason, write
NO DEMO for it.
The reader then answers each choice from what the page shows at open. (4) In which numbered
section did you first become clear on what the problem is? (5) Name up to five points you
had to re-read to follow the page.
(6) The chat message that triggered this page was: "<trigger message verbatim>".
Does the page answer that message, every part of it?
(7) Read as an engineer who knows general engineering and ML and the established CharlieBot
concepts (CharlieBot, manager, worker, verify, take off, understanding, plan, and sitrep pages),
and who wrote the trigger message in (6), so its terms are the reader's own. List only each
task-specific acronym or opaque identifier whose meaning the page never states, or states only
in a separate list or table. A term the page quotes as an object of discussion is a mention and
stands as quoted.
Write none when there is none.
(8) For a page that reports a run: does the page state what that run changed and what it
held fixed, each with a value? Name each factor and its value, or write NOT STATED for a
part the page does not state.
<!-- section: ask_system_prompt -->
You are a careful cold reader. Answer every question from the request and its linked page alone.
<!-- section: ask_questions -->
You are reading one request to a user cold. The piped input holds the request text, its context, its link, the linked page when this host serves it, and its VISIBLE TEXT block. You have no other context. Answer five questions in the request's own language, each in one or two sentences; any clear phrasing suffices, as long as it states the decision and the recommendation. When the input does not give an answer, write NOT STATED.
(1) What must the user decide or do?
(2) What does the line of work do now, and why does the decision come now?
(3) What are the options, what is the consequence of each, and which option does the request recommend? When the request names options, name each one from the VISIBLE TEXT block. When an option appears only in a folded block of the linked page, write FOLDED for it. When the options differ in what the reader sees and an option shows neither a drawn demo nor a stated reason, write NO DEMO for it on the linked page.
The request then gets each option from what the page shows at open.
(4) Which links carry the evidence?
(5) Which kind of step does the request ask about? Choose one:
- an undoable step: branch code, a page, a draft, a local file, a plan, or a design choice that needs no explicit user approval;
- a consequence step: deleting data without a backup, a message in the user's name or a publication, a merge into a shared main branch, a service restart, a job on a shared cluster, or spending money;
- an explicit user approval that a rule requires, where the request names the rule;
- a fact or an action that only the user has;
- the review of a delivered product.
After the five answers, tier the request against the Approval policy table section that the input carries. Check the table's consequence tests first: a step one of them names is t0, and the rule is that test's id. A step of a standing policy's kind that falls outside that policy's scope or over its cap is t0, and the rule is out-of-scope:<policy id>. A step inside a standing policy's scope is t1, and the rule is that policy's id. A reversible step in the user's own space that no standing policy covers, a design choice, or a reading below the table's reading test is t2, and the rule is none. A fact or an action that only the user has is user-act, and the rule is none. The review of a delivered product is review, and the rule is none. End the answer with one last line of exactly the form "tier: <tier> rule: <rule>", where <tier> is t0, t1, t2, user-act or review and <rule> is a consequence test id, out-of-scope:<policy id>, a standing policy id, or none.
