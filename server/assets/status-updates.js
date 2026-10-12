(() => {
  const root = document.querySelector("[data-status-update-url]");
  if (!root) return;

  const statusClasses = ["up", "slow", "down", "pending"];
  const intervalMs = 30_000;
  let requestInProgress = false;

  const setText = (element, value) => {
    if (element && element.textContent !== String(value ?? "")) {
      element.textContent = String(value ?? "");
    }
  };

  const setStatus = (element, statusClass, label) => {
    if (!element) return;
    const currentStatus = statusClasses.find((name) => element.classList.contains(name));
    if (currentStatus !== statusClass) {
      for (const name of statusClasses) element.classList.remove(name);
      if (statusClasses.includes(statusClass)) element.classList.add(statusClass);
    }
    setText(element.querySelector("[data-live-status-label]"), label);
    if (element.matches("[data-live-monitor-status]")) setText(element, label);
  };

  const setCounts = (rootElement, data) => {
    if (!rootElement) return;
    for (const [name, key] of [["normal", "normal_count"], ["down", "down_count"], ["slow", "slow_count"], ["pending", "pending_count"], ["total", "status_total_count"]]) {
      setText(rootElement.querySelector('[data-live-count="' + name + '"]'), data[key]);
    }
  };

  const updateUptimeBars = (scope, data) => {
    if (!scope) return;
    const bars = scope.matches?.("[data-uptime-bar]")
      ? [scope]
      : Array.from(scope.querySelectorAll("[data-uptime-bar]"));
    for (const bar of bars) {
      const segments = data.uptime_bar || [];
      const existing = Array.from(bar.querySelectorAll(".uptime-segment"));
      if (existing.length !== segments.length) {
        const fragment = document.createDocumentFragment();
        for (const segment of segments) fragment.append(createUptimeSegment(segment));
        bar.replaceChildren(fragment);
      } else {
        segments.forEach((segment, index) => updateUptimeSegment(existing[index], segment));
      }
      bar.setAttribute("aria-label", data.uptime_bar_label || "Uptime");
      const axis = bar.nextElementSibling;
      setText(axis?.querySelector("[data-uptime-start]"), data.uptime_bar_start_label);
      setText(axis?.querySelector("[data-uptime-end]"), data.uptime_bar_end_label);
    }
  };

  const updateUptimeSegment = (element, segment) => {
    const className = "uptime-segment " + segment.status_class;
    const title = segment.title || "";
    if (element.className !== className) element.className = className;
    if (element.title !== title) element.title = title;
    if (element.dataset.tooltip !== title) element.dataset.tooltip = title;
    if (element.getAttribute("aria-label") !== title) element.setAttribute("aria-label", title);
  };

  const createUptimeSegment = (segment) => {
    const element = document.createElement("span");
    element.tabIndex = 0;
    element.setAttribute("role", "img");
    updateUptimeSegment(element, segment);
    return element;
  };

  const updateDashboard = (data) => {
    const overall = document.querySelector("[data-live-overall]");
    if (overall) {
      setStatus(overall, data.overall_status_class, data.overall_status_label);
      setCounts(overall, data);
    }
    setText(document.querySelector("[data-generated-at]"), "Updated " + data.generated_at);

    const groups = new Map((data.groups || []).map((group) => [group.name, group]));
    for (const groupElement of document.querySelectorAll(".monitor-group[data-group-name]")) {
      const group = groups.get(groupElement.dataset.groupName);
      if (!group) continue;
      setText(groupElement.querySelector("[data-group-total]"), group.total_count + " services");
      const summary = groupElement.querySelector("[data-group-summary]");
      if (summary) {
        const groupStatus = summary.querySelector(".group-status");
        setStatus(groupStatus, group.status_class, group.status_label);
        setCounts(summary, group);
        updateUptimeBars(summary, group);
      }

      const monitors = new Map((group.monitors || []).map((monitor) => [monitor.id, monitor]));
      for (const card of groupElement.querySelectorAll(".monitor-card[data-monitor-id]")) {
        const monitor = monitors.get(card.dataset.monitorId);
        if (!monitor) continue;
        setStatus(card.querySelector("[data-live-monitor-status]"), monitor.status_class, monitor.status_label);
        setText(card.querySelector("[data-live-normal-label]"), monitor.normal_label);
        setText(card.querySelector("[data-live-normal-percent]"), monitor.normal_percent);
        updateUptimeBars(card, monitor);
      }
    }
  };

  const createHistoryRowCells = (rowData, isHttp, isTcp) => {
    const cells = [];
    const addCell = (label, value) => {
      const cell = document.createElement("td");
      cell.dataset.label = label;
      setText(cell, value);
      cells.push(cell);
      return cell;
    };
    addCell("Time", rowData.checked_at);

    const statusCell = addCell("Status", "");
    const status = document.createElement("span");
    status.className = "status " + rowData.status_class;
    setText(status, rowData.status_label);
    statusCell.append(status);

    addCell("Response", rowData.response_time);
    if (isHttp) addCell("Code", rowData.status_code);
    addCell("Message", rowData.message);
    if (isTcp) {
      const responseCell = addCell("TCP Response", "");
      if (rowData.tcp_response) {
        const details = document.createElement("details");
        details.className = "json-details";
        const summary = document.createElement("summary");
        setText(summary, "Show");
        const response = document.createElement("pre");
        setText(response, rowData.tcp_response);
        details.append(summary, response);
        responseCell.append(details);
      }
      if (rowData.tcp_response_truncated) {
        const truncated = document.createElement("span");
        truncated.className = "subtext";
        setText(truncated, "Truncated");
        responseCell.append(truncated);
      }
    }
    return cells;
  };

  const updateHistory = (data) => {
    const frame = document.querySelector("[data-history-table]");
    const empty = document.querySelector("[data-history-empty]");
    const body = frame?.querySelector("[data-history-body]");
    if (!frame || !body) return;
    const rows = data.history || [];
    const isHttp = frame.dataset.historyHttp === "true";
    const isTcp = frame.dataset.historyTcp === "true";
    const oldRows = new Map(Array.from(body.rows).map((row) => [row.dataset.checkedAtMs, row]));

    rows.forEach((rowData, index) => {
      const key = String(rowData.checked_at_ms);
      let row = oldRows.get(key);
      const snapshot = JSON.stringify(rowData);
      if (!row) {
        row = document.createElement("tr");
        row.dataset.checkedAtMs = key;
      }
      if (row.dataset.rowSnapshot !== snapshot) {
        row.replaceChildren(...createHistoryRowCells(rowData, isHttp, isTcp));
        row.dataset.rowSnapshot = snapshot;
      }
      const current = body.children[index];
      if (current !== row) body.insertBefore(row, current || null);
      oldRows.delete(key);
    });
    for (const row of oldRows.values()) row.remove();
    frame.hidden = rows.length === 0;
    if (empty) empty.hidden = rows.length !== 0;
  };

  const updateDetail = (data) => {
    const monitor = data.monitor;
    setStatus(document.querySelector("[data-live-monitor-status]"), monitor.status_class, monitor.status_label);
    setText(document.querySelector("[data-live-response-time]"), monitor.response_time);
    setText(document.querySelector("[data-live-normal-label]"), monitor.normal_label);
    setText(document.querySelector("[data-live-normal-percent]"), monitor.normal_percent);
    setText(document.querySelector("[data-live-last-checked]"), monitor.last_checked);
    updateUptimeBars(document.querySelector('[data-uptime-bar="overview"]'), monitor);
    updateUptimeBars(document.querySelector('[data-uptime-bar="checks"]'), {
      uptime_bar: data.recent_checks_bar,
      uptime_bar_label: data.recent_checks_bar_label,
      uptime_bar_start_label: data.recent_checks_bar_start_label,
      uptime_bar_end_label: data.recent_checks_bar_end_label,
    });

    const messagePanel = document.querySelector("[data-current-message]");
    if (messagePanel) {
      setText(messagePanel.querySelector("[data-current-message-text]"), monitor.message);
      messagePanel.hidden = !monitor.message;
    }

    const tcpPanel = document.querySelector("[data-tcp-response-panel]");
    if (tcpPanel) {
      const response = data.latest_tcp_response || "";
      const responseElement = tcpPanel.querySelector("[data-tcp-response-text]");
      const emptyElement = tcpPanel.querySelector("[data-tcp-response-empty]");
      const truncatedElement = tcpPanel.querySelector("[data-tcp-response-truncated]");
      setText(responseElement, response);
      if (responseElement) responseElement.hidden = !response;
      if (emptyElement) emptyElement.hidden = Boolean(response) || !data.has_tcp_check;
      if (truncatedElement) truncatedElement.hidden = !data.latest_tcp_response_truncated;
      tcpPanel.hidden = !response && !data.has_tcp_check;
    }

    updateHistory(data);
    window.dispatchEvent(new CustomEvent("status:update", {
      detail: {
        chartWindowEndMs: data.chart_window_end_ms,
        chartRetentionStartMs: data.chart_retention_start_ms,
      },
    }));
  };

  const refresh = async () => {
    if (requestInProgress) return;
    if (document.visibilityState === "visible") {
      requestInProgress = true;
      try {
        const response = await fetch(root.dataset.statusUpdateUrl, {
          headers: { Accept: "application/json" },
          cache: "no-store",
        });
        if (!response.ok) throw new Error("Status update failed: " + response.status);
        const data = await response.json();
        if (data.groups) updateDashboard(data);
        else if (data.monitor) updateDetail(data);
      } catch (error) {
        console.warn("Could not refresh status", error);
      } finally {
        requestInProgress = false;
      }
    }
    window.setTimeout(refresh, intervalMs);
  };

  window.setTimeout(refresh, intervalMs);
})();
