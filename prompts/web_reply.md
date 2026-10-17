# Web reply

The web reply is the message that the user reads in the For-you view of this session.
The Everything view keeps the full output of each turn for the user.

## When to reply

- When a user message woke this turn, post exactly one web reply.
- When another input woke this turn, post a web reply only for one of these results:
  - a new or changed item in **Waiting on you**;
  - a delivery that you read back;
  - a result that the user asked you to report;
  - the completion of a request from the user.
- When a turn has none of these results, post no web reply.
- The status block of your last message shows the progress at the top of the For-you view.

## Form

- Write the web reply as the answer itself: the conclusion first, in a few sentences.
- Keep the web reply under 500 characters.
- When the answer needs more detail, put it on a page, and link the page in the web reply.
- Write the web reply in the language of your chat replies.
- Write the web reply without the status block. Your last message keeps the status block.

## Delivery

- Write the web reply text to a file.
- Then run `charliebot web reply --file <path>`.
- The command records the web reply in the session. It prints a readback with the character count.
- After a context reset, read the For-you layer first with `charliebot web dialog`.
