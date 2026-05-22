const { invoke } = window.__TAURI__.core;

const state = {
  files: [],
  rows: [],
  currentScanningFile: "",
  rowStatus: {}
};

const pickBtn = document.getElementById("pick-btn");
const scanBtn = document.getElementById("scan-btn");
const exportBtn = document.getElementById("export-btn");
const timeoutInput = document.getElementById("timeout-secs");
const resultBody = document.getElementById("result-body");
const statusBox = document.getElementById("status");
const progressBar = document.getElementById("scan-progress");
const progressText = document.getElementById("progress-text");

let scanning = false;

function setScanning(flag) {
  scanning = flag;
  pickBtn.disabled = flag;
  scanBtn.disabled = flag;
  timeoutInput.disabled = flag;
}

function setProgress(percent) {
  const safe = Math.max(0, Math.min(100, percent));
  progressBar.value = safe;
  progressText.textContent = `${safe}%`;
}

function setStatus(text, kind = "") {
  statusBox.textContent = text;
  statusBox.className = `status ${kind}`.trim();
}

function getTimeoutSecs() {
  const raw = Number(timeoutInput.value || 5);
  if (!Number.isFinite(raw)) {
    return 5;
  }
  return Math.max(1, Math.min(180, Math.floor(raw)));
}

function renderRows() {
  resultBody.innerHTML = "";

  if (state.files.length === 0) {
    const tr = document.createElement("tr");
    const td = document.createElement("td");
    td.colSpan = 10;
    td.textContent = "请先选择文件";
    tr.appendChild(td);
    resultBody.appendChild(tr);
    return;
  }

  for (const file of state.files) {
    const row = state.rows.find((item) => item.file === file) || {};
    const status = state.rowStatus[file] || "idle";
    const waitingText = status === "processing" ? "识别中" : "";
    const queuedText = status === "queued" ? "待识别" : "";
    const fallbackText = waitingText || queuedText;
    const tr = document.createElement("tr");

    const fileTd = document.createElement("td");
    fileTd.textContent = file;
    tr.appendChild(fileTd);

    const typeTagTd = document.createElement("td");
    typeTagTd.textContent = row.invoice_type_tag || fallbackText;
    tr.appendChild(typeTagTd);

    const kindCodeTd = document.createElement("td");
    kindCodeTd.textContent = row.invoice_kind_code || fallbackText;
    tr.appendChild(kindCodeTd);

    const invoiceCodeTd = document.createElement("td");
    invoiceCodeTd.textContent = row.invoice_code || fallbackText;
    tr.appendChild(invoiceCodeTd);

    const invoiceNoTd = document.createElement("td");
    invoiceNoTd.textContent = row.invoice_number || fallbackText;
    tr.appendChild(invoiceNoTd);

    const amountTd = document.createElement("td");
    amountTd.textContent = row.issue_amount || fallbackText;
    tr.appendChild(amountTd);

    const dateTd = document.createElement("td");
    dateTd.textContent = row.issue_date || fallbackText;
    tr.appendChild(dateTd);

    const checkCodeTd = document.createElement("td");
    checkCodeTd.textContent = row.invoice_check_code || fallbackText;
    tr.appendChild(checkCodeTd);

    const encryptedTd = document.createElement("td");
    encryptedTd.textContent = row.encrypted_text || fallbackText;
    tr.appendChild(encryptedTd);

    const actionTd = document.createElement("td");
    const delBtn = document.createElement("button");
    delBtn.className = "btn";
    delBtn.textContent = "删除";
    delBtn.disabled = scanning;
    delBtn.addEventListener("click", () => {
      if (scanning) {
        return;
      }
      state.files = state.files.filter((p) => p !== file);
      state.rows = state.rows.filter((item) => item.file !== file);
      delete state.rowStatus[file];
      renderRows();
      setStatus(`已删除文件，剩余 ${state.files.length} 个`, "ok");
    });
    actionTd.appendChild(delBtn);
    tr.appendChild(actionTd);

    resultBody.appendChild(tr);
  }
}

pickBtn.addEventListener("click", async () => {
  try {
    const files = await invoke("pick_files");
    const incoming = Array.isArray(files) ? files : [];
    const merged = new Set(state.files);
    for (const file of incoming) {
      merged.add(file);
      if (!state.rowStatus[file]) {
        state.rowStatus[file] = "idle";
      }
    }
    state.files = Array.from(merged);
    renderRows();
    setStatus(`已选择 ${state.files.length} 个文件（可继续新增）`, "ok");
  } catch (e) {
    setStatus(`选择文件失败: ${e}`, "error");
  }
});

scanBtn.addEventListener("click", async () => {
  if (scanning) {
    return;
  }

  if (state.files.length === 0) {
    setStatus("请先选择文件", "error");
    return;
  }

  const timeoutSecs = getTimeoutSecs();
  timeoutInput.value = String(timeoutSecs);

  setStatus(`正在识别，请稍候... 单文件超时 ${timeoutSecs}s`);
  setProgress(0);
  setScanning(true);
  state.currentScanningFile = "";
  state.rows = [];
  state.rowStatus = {};
  for (const file of state.files) {
    state.rowStatus[file] = "queued";
  }
  renderRows();

  try {
    const total = state.files.length;
    let processed = 0;

    for (const file of state.files) {
      state.currentScanningFile = file;
      state.rowStatus[file] = "processing";
      setProgress(Math.floor(((processed + 0.4) / total) * 100));
      renderRows();
      setStatus(`处理中 ${processed}/${total}：${file}`);

      let first;
      try {
        first = await invoke("scan_invoice_qr_one", {
          file,
          timeoutSecs
        });
      } catch (_) {
        first = {
          file,
          invoice_type_tag: "识别失败",
          invoice_kind_code: "识别失败",
          invoice_code: "识别失败",
          invoice_number: "识别失败",
          issue_amount: "识别失败",
          issue_date: "识别失败",
          invoice_check_code: "识别失败",
          encrypted_text: "识别失败"
        };
      }

      state.rows = state.rows.filter((item) => item.file !== file);
      state.rows.push(first);
      state.rowStatus[file] = "done";

      processed += 1;
      setProgress(Math.floor((processed / total) * 100));
      setStatus(`处理中 ${processed}/${total}，本文件完成：${file}`);
      renderRows();
    }

    state.currentScanningFile = "";
    setStatus(`识别完成，共 ${state.rows.length} 条二维码`, "ok");
  } catch (e) {
    setStatus(`识别失败: ${e}`, "error");
  } finally {
    state.currentScanningFile = "";
    setScanning(false);
  }
});

exportBtn.addEventListener("click", async () => {
  if (scanning) {
    setStatus("识别进行中，请稍后再导出", "error");
    return;
  }

  if (state.rows.length === 0) {
    setStatus("没有可导出的识别结果", "error");
    return;
  }

  try {
    const path = await invoke("export_csv", { rows: state.rows });
    setStatus(`导出成功: ${path}`, "ok");
  } catch (e) {
    setStatus(`导出失败: ${e}`, "error");
  }
});

renderRows();
setProgress(0);
