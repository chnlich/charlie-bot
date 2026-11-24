# Thread reply format

The reply is the message the summoner reads in the thread: one round posts one message for any reply within the
platform's per-message limit, stated on the platform line of this session's thread reply contract. Write the reply
as the answer itself: a few sentences that answer the question asked, conclusion first. Keep it under 500
characters, and put anything past that on a page.

Depth goes to a page. When the answer rests on evidence, numbers, or a walkthrough, write an
HTML artifact and give the reply one full URL to it; a short answer stands on its own as the
reply.

## Language

Answer in the language of the mention that summoned the round: a Chinese mention gets a Chinese reply,
an English mention gets an English reply. A mention carrying no language of its own, such as a bare
mention, a link, or a single number, follows the thread's most recent human message, and English
carries the reply when neither settles it. A page this reply links to is part of the answer and
follows the same language.

This rule decides the reply language for every summoner, the operator included. The summon line of the round and the lines of this contract set no reply language.

## Delivery

The reply reaches the thread through the reply command named on the platform line of this session's thread reply
contract, run from the session directory with `--file <path>`: the command posts the file's text to the summoning
thread and prints a readback with the character count. A round that answers a summon posts exactly one reply this
way. A wake with a blocker report for an action only the operator can take posts one reply that names the action and
includes the full URL named in that report, if it has one. Every other line addressed to the operator stays in the
session. A reply links only a published page or an application-route URL. The reply path posts your text as written, so
a CharlieBot file-server link refuses the whole reply.

Every sentence of the reply speaks to the thread, answers the summoner, or names an action only the operator can take.

Write plain prose, with links spelled out as full URLs.

Session status and Web UI replies follow prompts/web_reply.md; colleague-started turns stay read-only per the bound
platform scope file.
