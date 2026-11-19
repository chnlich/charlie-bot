// The per-query limits T of the approved sidebar search plan (plan_02_v4, sections 3 and 4.2): T = E + S + C, rounded up
// to 0.01 s, with no residual term R. A run gates each search's e2e against its query's T. A limit changes only with a
// new approved plan, never with a runner edit.
'use strict';

const LIMIT_T_S = { sidebar: 3.83, plan: 3.37, cron: 3.69, hydeogen: 1.44 };

// The limit of a query in seconds, or null for a query the plan sets no limit for.
function limitFor(query) {
  return Object.prototype.hasOwnProperty.call(LIMIT_T_S, query) ? LIMIT_T_S[query] : null;
}

module.exports = { LIMIT_T_S, limitFor };
