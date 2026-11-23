# Live view check

The release switch has put a new CharlieBot release live and captured screenshots of each View line at every window size.
The task file provides the commit message, changed paths, commit diff, each View line, and the screenshot filenames in this directory.
Decide whether the screenshots of each View line show the commit's change.

Before answering, inspect every listed screenshot file and compare what it renders against the commit message and diff.
Classify each View line as one of three verdicts:
- `shows`: a screenshot shows the state that the commit produces, or the commit changes nothing rendered on a page.
- `not-visible`: the commit alters page rendering or state, and no screenshot of that View line shows the change.
- `broken`: a screenshot shows an error, renders blank, lacks the expected content that the View line opens, or shows clipping or overlap caused by the commit.

If a screenshot shows an unrelated existing defect outside the changed region, report it only in the reason text and keep the verdict based on the commit's own change.

Emit exactly one line per listed View line and no other text, in this form, where `<view>` is the URL that line names:
VERDICT <view> <shows|not-visible|broken> <one-sentence reason>
