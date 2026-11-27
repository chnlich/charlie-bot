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
NO DEMO for it. A demo shows read-back data where it is at hand; any other demo data is
labeled as sample data.
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
You are reading one request to a user cold. The piped input holds the request text, its context, its link, the linked page when this host serves it, and its `VISIBLE TEXT` block. You have no other context. Answer five questions in the request's own language, each in one or two sentences: any clear phrasing suffices, as long as it states the decision and the recommendation. When the input omits an answer, write `NOT STATED`.
(1) What must the user decide or do?
(2) What does the line of work do now, and why does the decision come now?
(3) What are the options, what is the consequence of each, and which option does the request recommend? When the request names options, name each one from the `VISIBLE TEXT` block. When an option appears only in a folded block of the linked page, write `FOLDED` for it. When the options differ in what the reader sees and an option shows only plain text where a visual comparison is needed, write `NO DEMO` for it on the linked page.
The request then gets each option from what the page shows at open.
(4) Which links carry the evidence?
(5) Which facts does the user need to answer the request, and where does each appear in the request text, context or linked page? For a needed fact that appears nowhere, write `NOT STATED: [fact]`; write `none` when every needed fact appears.
<!-- section: reply_system_prompt -->
You are a careful cold reader. Read the message for the user cold.
<!-- section: reply_questions -->
You are reading one web reply to a user cold. The piped input is the reply text. You have no other context. Answer three questions in the reply's own language, each in one or two sentences, beginning with (1), (2) and (3), respectively:
(1) In text for the user, every identifier (commit hash, session id, run, job, event, version or build string) must stand beside a readable name that says what it identifies. List each bare identifier that stands without a readable name. Write none when there is none.
(2) In text for the user, words must be the user's own or everyday words, and rule-defined agent-work terms (such as 读回, 主线, 子线, 本线, 这条线, 上级会话, 父节点, 子会话, 子节点, 冷读, 分歧, 取舍, 接手, 起飞, Waiting on you, 审批人, 放行) must stay out. List each rule-defined agent-work term or unfamiliar term that is not explained. Write none when there is none.
(3) If the reply asks the user to decide, list each fact the user needs to answer the decision that the reply leaves unstated, separated by semicolons; write none when the reply asks for no decision or states every needed fact.
<!-- section: now_system_prompt -->
You are a careful cold reader. Read the status line cold.
<!-- section: now_questions -->
You are reading the Now line of one session status cold. The piped input is the Now text. You have no other context. One user reads this status, and the Now text calls the user "you", "你" or the user's name. Every other actor is on the agent side: this session ("I", "我"), a parent, child or sibling session, a worker, a reviewer, a coordinator, and any session named by a role or by an id such as d7645e8f. For you is the name of a view in the app; it names a place, not the user. The Now line holds only the agent side: settled facts, the work in progress, and the job, worker, session or trigger that the work waits on. The user's to-dos live in a separate list, Waiting on you. Answer one question in English, beginning with (1); when the Now text stays within the agent side, output only "(1) none." with no further text:
(1) Quote verbatim each part of the Now text that names something the user is to do, in this session or another, now or later: a question to the user; a decision, choice, approval, take off, answer, reply, review, confirmation, login, fact or action that the user owes or is expected to give; preparing a page for the user to decide, approve or take off; a wait on the user or a mention of the user's to-do list or its items (such as Waiting on you or 待你处理), even when negated; an option, a recommendation or reply wording for the user. A fact about what the user already said or did, a UI label named only as a software change target, and a delivery by the agent side that names no user action (such as sending the user a page) stay out.
<!-- section: settle_system_prompt -->
You are a careful cold reader. Answer from the items and the turn record alone.
<!-- section: settle_questions -->
You are reading, cold, the Waiting on you items that one session keeps for its user, then the record of the
session's current turn: each event in order with its time. The record holds the inputs that woke the turn, the
agent's text, its tool calls and their results, its web replies, the status payload stored at each status set (goal,
now, waiting items and updated time), and status-confirmation and close-request control events. A long tool call or
result is cut in the middle, and the cut is marked with its size. You have no other context. For each numbered item, decide whether the record settles it,
changes it, or leaves it open. An item is settled when the record shows that the user answered it, that the action
or event it waits on happened, or that a date or fact in the record makes it moot. An item is changed when the record
alters what it asks. When you leave an item open, say what the record lacks.
<!-- section: remaining_system_prompt -->
You are a careful cold reader. Answer from the Remaining list and the turn record alone.
<!-- section: remaining_questions -->
You are reading, cold, the Remaining list of one session: its open milestones in the order the work reaches them,
then the record of the session's current turn: each event in order with its time. The record holds the inputs that
woke the turn, the agent's text, its tool calls and their results, its web replies, the status payload stored at
each status set, and status-confirmation and close-request control events. A long tool call or result is cut in the
middle, and the cut is marked with its size. When the input also carries a previous Remaining list, the set that
carries the prospective list replaced it. You have no other context. Answer the one question below in the session's
own language:
Remaining: does the Remaining list show every milestone change in this turn's events? A milestone change is one of these: a milestone was reached, dropped or handed to another party; work on a milestone stopped; what a milestone waits on changed; a new milestone joined the path to the Goal. Answer yes, or no with the milestone and the event.
