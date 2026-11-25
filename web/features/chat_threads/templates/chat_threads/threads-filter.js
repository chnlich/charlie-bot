// The sidebar's Threads filter: the chat threads list, in front of the Later filter. The filter narrows the Workspace
// to the thread-bound sessions and removes none of them from it.
registerSidebarFilter({name: 'threads', label: 'Threads', url: '/api/sessions/chat-threads', before: 'starred'});
// A thread-bound row carries its platform badge on its title line, after the name.
registerSessionRowTitleBadge((s) => (s.channel_platform ? Chat.platformBadgeHtml(s.channel_platform) : ''));
