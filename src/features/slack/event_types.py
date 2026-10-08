"""Event wire names that the Slack package owns."""

# A reply the master posted to its session's Slack thread through
# ``charliebot slack reply``; the same-named ``slack_reply`` payload names the
# summon it answers, which the round-end audit reads. Both uses share this one
# constant (the thread platform's ``reply_event_type`` is the event type and the payload key).
SLACK_REPLY = "slack_reply"
