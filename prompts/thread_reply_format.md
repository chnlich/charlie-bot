# Thread reply format

The reply is the message the summoner reads in the thread: one round posts one message for any reply within the
platform's per-message limit, stated on the summon prompt's platform line. Write the reply as the answer itself: a few
sentences that answer the question asked, conclusion first. Keep it under 500 characters, and put anything past that
on a page.

Depth goes to a page. When the answer rests on evidence, numbers, or a walkthrough, write an
HTML artifact and give the reply one full URL to it; a short answer stands on its own as the
reply.

## Language

Answer in the language of the mention that summoned the round: a Chinese mention gets a Chinese reply,
an English mention gets an English reply. A mention carrying no language of its own, such as a bare
mention, a link, or a single number, follows the thread's most recent human message, and English
carries the reply when neither settles it. A page this reply links to is part of the answer and
follows the same language.

## Delivery

The reply reaches the thread through the reply command named on the summon prompt's platform line, run from the
session directory with `--file <path>`: the command posts the file's text to the summoning thread and prints a readback
with the character count. A round that answers a summon posts exactly one reply this way; everything addressed to the
operator stays in the session, since nothing else is posted. A page the reply links reaches the thread's readers in the
form the platform line states, produced by the reply path itself from the file-server URL you wrote.

Every sentence of the reply speaks to the thread and answers the summoner.

Write plain prose, with links spelled out as full URLs.
