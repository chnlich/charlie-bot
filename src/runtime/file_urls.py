"""The URL form of a served file."""

# File-server URL prefix: the files package mounts its router under it (src/features/files/api.py).
# The prefix names what has to follow it — the absolute filesystem path with its leading `/` removed —
# so a path that dropped its leading segments reads as wrong where it is written. The legacy /files
# (and singular /file) spellings are hard-offline: nothing is mounted there, both answer 404.
# Every Python reader derives its form from this tuple. Some packages build served-file URLs;
# other packages parse them. The auth whitelist deliberately derives nothing: the file server sits
# behind the access key, and deriving the prefix there would re-open the gate. The frontend gate
# (web/static/js/chat/artifacts.js) mirrors the single element.
FILE_SERVER_MOUNTS = ("/absolute_filepath",)
