import { execFileSync } from 'node:child_process';
const uuid = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
let next = 300;
const catalog = ['Bills', 'Everyday', 'Fun', 'Travel'].map((g, gi) => ({
  id: uuid(next++), name: g,
  categories: Array.from({ length: 10 }, (_, i) => ({ id: uuid(next++), name: `${g} item ${String(i + 1).padStart(2, '0')}` })),
}));
const code = `async () => {
  await codemode.create_account({ id: "${uuid(201)}", name: "Scroll checking", institution: null, unit: { id: "${uuid(101)}", name: "US dollar", symbol: "USD", kind: "fiat", convention: "none" }, participation: "on_plan", opening: "5000.00", startDate: "2026-08-01" });
  let revision = (await codemode.read_category_catalog({})).revision;
  for (const group of ${JSON.stringify(catalog)}.reverse()) {
    revision = (await codemode.create_category_group({ id: group.id, name: group.name, revision })).revision;
    for (const c of [...group.categories].reverse())
      revision = (await codemode.create_category({ id: c.id, groupId: group.id, name: c.name, revision })).revision;
  }
  for (const [n, amount, note] of [[901,"12.00","Coffee"],[902,"40.00","Groceries run"],[903,"9.99","Streaming"]])
    await codemode.create_operation({ id: "00000000-0000-4000-9000-" + String(n).padStart(12,"0"), accountId: "${uuid(201)}", type: "expense", amount, date: "2026-09-1" + (n-900), note, destinationId: null, receivedAmount: null, category: null });
  return (await codemode.read_category_catalog({})).revision;
}`;
process.stdout.write(execFileSync('node', ['scripts/app-driver.mjs', 'mcp', '--run-id', process.argv[2], '--feature', 'assistant.split-create', '--action', JSON.stringify({ operation: 'call', code })], { encoding: 'utf8' }));
