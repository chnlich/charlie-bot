"""Event wire names that the Discord package owns."""

# A reply the master posted to its session's Discord thread through
# ``charliebot discord reply``; the same-named ``discord_reply`` payload names
# the summon it answers, which the round-end audit reads. Both uses share this
# one constant (the thread platform's ``reply_event_type`` is the event type and the payload key).
DISCORD_REPLY = "discord_reply"
