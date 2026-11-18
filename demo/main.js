// The page of the browser demo. The decisions come from `decide_scenario` (crates/demo-wasm), the server's
// rules compiled to WebAssembly; this file only collects the input and words the result.

import init, { decide_scenario } from './pkg/payment_matching_demo.js';

const PRESETS = [
  {
    label: 'README の例',
    note: '合算入金は自動確定、手数料の差引と名義不明は候補、どちらも合わない入金は未消込になります。',
    customers: [{ name_kana: 'カブシキガイシャ ヤマダショウジ', fee_deducted: true, aliases: '' }],
    invoices: [
      { customer: 0, number: 'INV-001', amount: '110000', due_date: '2026-09-30' },
      { customer: 0, number: 'INV-002', amount: '55000', due_date: '2026-10-31' },
      { customer: 0, number: 'INV-003', amount: '33000', due_date: '2026-11-30' },
      { customer: 0, number: 'INV-004', amount: '220000', due_date: '2026-11-30' },
    ],
    payments: [
      { payer_name: 'ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ', amount: '165000' },
      { payer_name: 'ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ', amount: '219560' },
      { payer_name: 'ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ', amount: '33000' },
      { payer_name: 'ﾌﾒｲ ｼﾖｳｶｲ', amount: '5000' },
    ],
  },
  {
    label: '同額の請求が 2 件',
    note: '同じ金額の請求が 2 件あると、どちらの支払いか決められないので、自動確定せず古いほうを候補にします。請求を 1 件消すと自動確定に変わります。',
    customers: [{ name_kana: 'スズキコウギョウ', fee_deducted: false, aliases: '' }],
    invoices: [
      { customer: 0, number: 'INV-101', amount: '55000', due_date: '2026-10-31' },
      { customer: 0, number: 'INV-102', amount: '55000', due_date: '2026-11-30' },
    ],
    payments: [{ payer_name: 'ｽｽﾞｷｺｳｷﾞﾖｳ(ｶ', amount: '55000' }],
  },
  {
    label: '後の入金が候補を取り消す',
    note: '1 行目は同額の請求が 2 件あるので候補どまり。2 行目が 3 件すべてを消し込むと、1 行目の候補は確定できなくなるので取消になり、未消込に戻ります。',
    customers: [{ name_kana: 'ヤマダショウジ', fee_deducted: false, aliases: '' }],
    invoices: [
      { customer: 0, number: 'INV-201', amount: '55000', due_date: '2026-10-31' },
      { customer: 0, number: 'INV-202', amount: '55000', due_date: '2026-11-30' },
      { customer: 0, number: 'INV-203', amount: '33000', due_date: '2026-11-30' },
    ],
    payments: [
      { payer_name: 'ﾔﾏﾀﾞｼﾖｳｼﾞ', amount: '55000' },
      { payer_name: 'ﾔﾏﾀﾞｼﾖｳｼﾞ', amount: '143000' },
    ],
  },
  {
    label: '別名で取引先を特定する',
    note: '支店名のついた名義は、別名に登録してあれば取引先を特定できます（1 行目）。登録のない名義は金額だけの一致になり、候補どまりです（2 行目）。',
    customers: [
      { name_kana: 'ヤマダショウジ', fee_deducted: false, aliases: 'ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ' },
      { name_kana: 'タナカサンギョウ', fee_deducted: false, aliases: '' },
    ],
    invoices: [
      { customer: 0, number: 'INV-301', amount: '88000', due_date: '2026-11-30' },
      { customer: 1, number: 'INV-302', amount: '44000', due_date: '2026-11-30' },
    ],
    payments: [
      { payer_name: 'ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ', amount: '88000' },
      { payer_name: 'ﾀﾅｶ ﾄｳｷﾖｳｴｲｷﾞﾖｳｼﾖ', amount: '44000' },
    ],
  },
];

const BLANK = {
  customers: () => ({ name_kana: '', fee_deducted: false, aliases: '' }),
  invoices: () => ({ customer: 0, number: '', amount: '', due_date: '2026-11-30' }),
  payments: () => ({ payer_name: '', amount: '' }),
};

const state = { customers: [], invoices: [], payments: [] };
const $ = (id) => document.getElementById(id);
const yen = (n) => `${Number(n).toLocaleString('ja-JP')} 円`;

/// Creates an element. Text goes in as text, never as HTML: everything shown here was typed by the visitor.
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === false || value == null) continue;
    if (name === 'class') node.className = value;
    else if (name === 'checked' || name === 'value' || name === 'hidden') node[name] = value;
    else node.setAttribute(name, value === true ? '' : value);
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child.nodeType ? child : document.createTextNode(String(child)));
  }
  return node;
}

function load(preset) {
  for (const table of ['customers', 'invoices', 'payments']) {
    state[table] = preset[table].map((row) => ({ ...row }));
  }
  $('preset-note').textContent = preset.note;
  renderInputs();
  renderResult();
}

// ---------------------------------------------------------------------------------------------------- input

function field(table, index, key, attrs) {
  return el('input', { 'data-table': table, 'data-index': index, 'data-key': key, ...attrs });
}

function removeButton(table, index, what) {
  return el('button', {
    type: 'button', class: 'remove', 'data-remove': table, 'data-index': index,
    'aria-label': `${what} ${index + 1} を削除`, title: '削除',
  }, '×');
}

function customerName(index) {
  const name = state.customers[index]?.name_kana.trim();
  return name ? name : `取引先 ${index + 1}`;
}

function renderInputs() {
  $('customers').replaceChildren(
    el('thead', {}, el('tr', {}, el('th', {}, 'カナ名'), el('th', {}, '手数料を差し引いて支払う'), el('th', {}, '別名'), el('th', {}))),
    el('tbody', {}, state.customers.map((row, i) => el('tr', {},
      el('td', {}, field('customers', i, 'name_kana', { type: 'text', value: row.name_kana, 'aria-label': `取引先 ${i + 1} のカナ名`, placeholder: 'ヤマダショウジ' })),
      el('td', { class: 'center' }, field('customers', i, 'fee_deducted', { type: 'checkbox', checked: row.fee_deducted, 'aria-label': `取引先 ${i + 1} は手数料を差し引いて支払う` })),
      el('td', {}, field('customers', i, 'aliases', { type: 'text', value: row.aliases, 'aria-label': `取引先 ${i + 1} の別名`, placeholder: 'ﾔﾏﾀﾞ ｵｵｻｶｼﾃﾝ' })),
      el('td', {}, removeButton('customers', i, '取引先')),
    ))),
  );

  $('invoices').replaceChildren(
    el('thead', {}, el('tr', {}, el('th', {}, '取引先'), el('th', {}, '請求番号'), el('th', {}, '金額（円）'), el('th', {}, '支払期日'), el('th', {}))),
    el('tbody', {}, state.invoices.map((row, i) => el('tr', {},
      el('td', {}, el('select', { 'data-table': 'invoices', 'data-index': i, 'data-key': 'customer', 'aria-label': `請求 ${i + 1} の取引先` },
        state.customers.map((_, c) => el('option', { value: c, selected: c === row.customer }, customerName(c))))),
      el('td', {}, field('invoices', i, 'number', { type: 'text', value: row.number, 'aria-label': `請求 ${i + 1} の番号`, placeholder: `INV-${String(i + 1).padStart(3, '0')}` })),
      el('td', {}, field('invoices', i, 'amount', { type: 'text', inputmode: 'numeric', value: row.amount, 'aria-label': `請求 ${i + 1} の金額` })),
      el('td', {}, field('invoices', i, 'due_date', { type: 'date', value: row.due_date, 'aria-label': `請求 ${i + 1} の支払期日` })),
      el('td', {}, removeButton('invoices', i, '請求')),
    ))),
  );

  $('payments').replaceChildren(
    el('thead', {}, el('tr', {}, el('th', {}, '行'), el('th', {}, '振込名義'), el('th', {}, '金額（円）'), el('th', {}))),
    el('tbody', {}, state.payments.map((row, i) => el('tr', {},
      el('td', { class: 'num' }, i + 1),
      el('td', {}, field('payments', i, 'payer_name', { type: 'text', value: row.payer_name, 'aria-label': `入金 ${i + 1} の振込名義`, placeholder: 'ｶ)ﾔﾏﾀﾞｼﾖｳｼﾞ' })),
      el('td', {}, field('payments', i, 'amount', { type: 'text', inputmode: 'numeric', value: row.amount, 'aria-label': `入金 ${i + 1} の金額` })),
      el('td', {}, removeButton('payments', i, '入金')),
    ))),
  );
}

document.addEventListener('input', (event) => {
  const { table, index, key } = event.target.dataset;
  if (!table) return;
  const row = state[table][Number(index)];
  if (event.target.type === 'checkbox') row[key] = event.target.checked;
  else if (key === 'customer') row[key] = Number(event.target.value);
  else row[key] = event.target.value;
  // The names of the customers are the options of the invoices.
  if (table === 'customers' && key === 'name_kana') {
    for (const select of document.querySelectorAll('select[data-key="customer"]')) {
      select.options[Number(index)].textContent = customerName(Number(index));
    }
  }
  renderResult();
});

document.addEventListener('click', (event) => {
  const { add, remove, index, preset } = event.target.dataset;
  if (preset != null) {
    load(PRESETS[Number(preset)]);
  } else if (add) {
    if (add === 'invoices' && state.customers.length === 0) state.customers.push(BLANK.customers());
    state[add].push(BLANK[add]());
    renderInputs();
    renderResult();
    document.querySelector(`#${add} tbody tr:last-child input`)?.focus();
  } else if (remove) {
    const at = Number(index);
    state[remove].splice(at, 1);
    if (remove === 'customers') {
      // The invoices of the customer go with it; the others keep theirs.
      state.invoices = state.invoices
        .filter((invoice) => invoice.customer !== at)
        .map((invoice) => ({ ...invoice, customer: invoice.customer > at ? invoice.customer - 1 : invoice.customer }));
    }
    renderInputs();
    renderResult();
  }
});

// --------------------------------------------------------------------------------------------------- result

/// Full-width digits and separators are accepted as people type them: "１１０，０００" is 110000.
function amountOf(text) {
  const plain = text.normalize('NFKC').replace(/[,\s円¥]/g, '');
  return /^\d{1,15}$/.test(plain) ? Number(plain) : NaN;
}

/// What goes to the rules: rows that are filled in. Returns the positions of those rows too, so that the
/// result can be shown against the rows on the page.
function scenario() {
  const problems = [];
  const invoices = [];
  const invoiceRows = [];
  state.invoices.forEach((row, i) => {
    if (row.amount.trim() === '') return;
    const amount = amountOf(row.amount);
    if (Number.isNaN(amount)) return problems.push(`請求 ${i + 1} の金額を数字で入力してください。`);
    if (row.due_date === '') return problems.push(`請求 ${i + 1} の支払期日を入力してください。`);
    invoices.push({ customer: row.customer, amount, due_date: row.due_date });
    invoiceRows.push(i);
  });
  const payments = [];
  const paymentRows = [];
  state.payments.forEach((row, i) => {
    if (row.amount.trim() === '' && row.payer_name.trim() === '') return;
    const amount = amountOf(row.amount);
    if (Number.isNaN(amount)) return problems.push(`入金 ${i + 1} の金額を数字で入力してください。`);
    payments.push({ payer_name: row.payer_name, amount });
    paymentRows.push(i);
  });
  const customers = state.customers.map((row) => ({
    name_kana: row.name_kana,
    fee_deducted: row.fee_deducted,
    aliases: row.aliases.split(/[、,\n]/).map((alias) => alias.trim()).filter(Boolean),
  }));
  return { input: { customers, invoices, payments }, invoiceRows, paymentRows, problems };
}

const PROBLEMS = { out_of_range: 'は 1 円以上、999,999,999,999 円以下にしてください。', invalid: 'の日付が正しくありません。', not_found: 'の取引先がありません。' };

function describeProblem(problem, rows) {
  const found = /^(invoices|payments)\[(\d+)\]\.(\w+)$/.exec(problem.field);
  if (!found) return `入力を読めませんでした（${problem.field}: ${problem.code}）。`;
  const [, table, index] = found;
  const row = (table === 'invoices' ? rows.invoiceRows : rows.paymentRows)[Number(index)] + 1;
  return `${table === 'invoices' ? '請求' : '入金'} ${row} ${PROBLEMS[problem.code] ?? `: ${problem.code}`}`;
}

function invoiceLabel(row) {
  const invoice = state.invoices[row];
  const number = invoice.number.trim() || `請求 ${row + 1}`;
  return `${number}（${yen(amountOf(invoice.amount))}）`;
}

const OUTCOME = {
  matched: ['自動確定', 'ok'],
  suggested: ['候補（人が確定）', 'wait'],
  unmatched: ['未消込', 'none'],
};

function explain(payment, rows) {
  const { decision } = payment;
  const names = decision.invoices.map((i) => invoiceLabel(rows.invoiceRows[i]));
  const who = payment.customer == null ? null : customerName(payment.customer);
  const lines = [];

  if (decision.outcome === 'auto_confirmed' && decision.rule === 'R1') {
    lines.push(`名義から ${who} と特定。同額の未消込請求は ${names[0]} の 1 件だけなので、自動で消し込みます。`);
  } else if (decision.outcome === 'auto_confirmed') {
    lines.push(`名義から ${who} と特定。未消込請求の合計が入金額になる組み合わせは ${names.join(' + ')} の 1 通りだけなので、自動で消し込みます。`);
  } else if (decision.outcome === 'suggested') {
    if (decision.reason === 'same_amount_invoices') {
      lines.push(`名義から ${who} と特定。同額の未消込請求が ${decision.count} 件あり、どれの支払いか決められないので、最も古い ${names[0]} を候補にします。`);
    } else if (decision.reason === 'several_combinations' && decision.rule === 'R2') {
      lines.push(`名義から ${who} と特定。合計が入金額になる組み合わせが複数あるので、古い請求を優先した ${names.join(' + ')} を候補にします。`);
    } else if (decision.reason === 'fee_deducted') {
      lines.push(`名義から ${who} と特定。入金額に振込手数料 ${yen(decision.fee_amount)} を足すと ${names.join(' + ')} に一致します。手数料の差引は、1 通りでも人が確定します。`);
    } else if (decision.reason === 'several_combinations') {
      lines.push(`名義から ${who} と特定。手数料を差し引いたとする説明が複数あるので、そのうち ${names.join(' + ')}（手数料 ${yen(decision.fee_amount)}）を候補にします。`);
    } else {
      const others = decision.candidates.length > 1
        ? `同額の未消込請求は ${decision.candidates.length} 件あり、最も古い ${names[0]} を候補にします。`
        : `同額の未消込請求 ${names[0]} を候補にします。`;
      lines.push(`名義が登録されていないので、取引先を特定できません。${others}金額しか一致していないので、人が確定します。`);
    }
  } else if (decision.reason === 'payer_unknown') {
    lines.push(payment.normalized == null
      ? '名義を正規化できず（法人格や記号だけ、など）、同額の未消込請求もありません。'
      : '名義が登録されておらず、同額の未消込請求もありません。');
  } else {
    lines.push(`名義から ${who} と特定しましたが、この金額を説明できる未消込請求がありません。`);
    if (decision.subset_search_skipped) lines.push('未消込請求が多いため、組み合わせの探索は省略しました。');
  }

  if (payment.lost_to != null) {
    lines.push(`ただし、${rows.paymentRows[payment.lost_to] + 1} 行目の入金がこの請求を消し込んだため、候補は取消になり、未消込に戻りました。`);
  }
  return lines;
}

function renderResult() {
  const rows = scenario();
  const errors = $('errors');
  const reply = JSON.parse(decide_scenario(JSON.stringify(rows.input)));
  const problems = [...rows.problems, ...(reply.errors ?? []).map((problem) => describeProblem(problem, rows))];
  errors.hidden = problems.length === 0;
  errors.replaceChildren(el('ul', {}, problems.map((text) => el('li', {}, text))));
  if (!reply.ok) {
    $('summary').textContent = '';
    $('results').replaceChildren();
    return;
  }
  const { payments, invoices, names, fee_amounts: fees } = reply.ok;

  const count = (status) => payments.filter((payment) => payment.status === status).length;
  $('summary').textContent = payments.length === 0
    ? '入金を入力すると、ここに結果が出ます。'
    : `入金 ${payments.length} 件：自動確定 ${count('matched')} 件、候補 ${count('suggested')} 件、未消込 ${count('unmatched')} 件`;

  const cards = payments.map((payment, i) => {
    const row = rows.paymentRows[i];
    const [label, tone] = OUTCOME[payment.status];
    const raw = state.payments[row].payer_name;
    return el('article', { class: `card ${tone}` },
      el('header', {},
        el('span', { class: 'line' }, `${row + 1} 行目`),
        el('span', { class: 'amount' }, yen(rows.input.payments[i].amount)),
        el('span', { class: `badge ${tone}` }, label),
        payment.decision.rule && el('span', { class: 'rule' }, payment.decision.rule),
      ),
      el('p', { class: 'name' },
        el('span', { class: 'raw' }, raw === '' ? '（名義なし）' : raw),
        ' → ',
        payment.normalized == null ? el('span', { class: 'muted' }, '正規化できません') : el('span', { class: 'norm' }, payment.normalized),
        payment.customer == null ? el('span', { class: 'muted' }, '（未登録）') : `（${customerName(payment.customer)}）`,
      ),
      explain(payment, rows).map((text) => el('p', { class: 'why' }, text)),
    );
  });

  const invoiceTable = invoices.length === 0 ? [] : [
    el('h3', {}, '取り込み後の請求'),
    el('div', { class: 'scroll' }, el('table', { class: 'plain' },
      el('thead', {}, el('tr', {}, el('th', {}, '請求'), el('th', {}, '取引先'), el('th', {}, '支払期日'), el('th', {}, '状態'))),
      el('tbody', {}, invoices.map((invoice, i) => {
        const row = rows.invoiceRows[i];
        return el('tr', {},
          el('td', {}, invoiceLabel(row)),
          el('td', {}, customerName(state.invoices[row].customer)),
          el('td', {}, state.invoices[row].due_date),
          el('td', {}, invoice.cleared_by == null
            ? el('span', { class: 'badge none' }, '未消込')
            : [el('span', { class: 'badge ok' }, '消込済み'), ` ${rows.paymentRows[invoice.cleared_by] + 1} 行目の入金`]),
        );
      })),
    )),
  ];
  $('results').replaceChildren(...cards, ...invoiceTable);

  const NAME_PROBLEMS = { not_normalizable: '正規化できないので登録できません', taken: '他の取引先（または同じ取引先）の名義と重なるので登録できません' };
  $('names').replaceChildren(
    el('thead', {}, el('tr', {}, el('th', {}, '取引先'), el('th', {}, '種類'), el('th', {}, '登録した名義'), el('th', {}, '照合に使う形'))),
    el('tbody', {}, names.filter((name) => name.raw.trim() !== '').map((name) => el('tr', {},
      el('td', {}, customerName(name.customer)),
      el('td', {}, name.source === 'kana' ? 'カナ名' : '別名'),
      el('td', {}, name.raw),
      el('td', {}, name.problem ? el('span', { class: 'warn' }, NAME_PROBLEMS[name.problem]) : el('span', { class: 'norm' }, name.normalized)),
    ))),
  );
  $('fees').textContent = `R3 が試す振込手数料：${fees.map((fee) => fee.toLocaleString('ja-JP')).join('、')} 円`;
}

// ---------------------------------------------------------------------------------------------------- start

try {
  await init();
  $('presets').replaceChildren(
    ...PRESETS.map((preset, i) => el('button', { type: 'button', 'data-preset': i }, preset.label)),
  );
  load(PRESETS[0]);
} catch (error) {
  console.error(error);
  const errors = $('errors');
  errors.hidden = false;
  errors.textContent = 'デモを読み込めませんでした。WebAssembly に対応したブラウザで開いてください。';
}
