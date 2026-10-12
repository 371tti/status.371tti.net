(() => {
  const page = document.querySelector(".response-time-panel[data-chart-data-url]");
  if (!page || typeof Chart === "undefined") return;

  const chartInstances = new Map();
  const chartRangeRequests = new Map();
  const responseShell = page.querySelector("[data-response-chart-shell]");
  const responseCanvas = responseShell?.querySelector("canvas.response-chart");
  const pointTooltip = document.getElementById("point-tooltip");
  const metricPanel = document.querySelector("[data-metric-panel]");
  const metricGroupsRoot = metricPanel?.querySelector("[data-metric-chart-groups]");
  const timingPanel = document.querySelector("[data-http-phase-timings]");
  const dateInput = page.querySelector("[data-chart-day]");
  const previousDayButton = page.querySelector("[data-day-previous]");
  const nextDayButton = page.querySelector("[data-day-next]");
  const resetZoomButton = page.querySelector("[data-chart-reset-zoom]");
  const loadError = page.querySelector("[data-chart-load-error]");
  const chartDataUrl = page.dataset.chartDataUrl;
  let retentionStartMs = Number(page.dataset.retentionStartMs);
  let windowEndMs = Number(page.dataset.windowEndMs);
  const DAY_MS = 86_400_000;
  const CHART_Y_AXIS_WIDTH = 64;
  const CHART_X_AXIS_HEIGHT = 40;
  const metricPalette = ["--accent", "#67c5b4", "#e4bf72", "#8bc6a1", "#c29bdd", "#e6aaaa", "#76bfe8", "#e2df84"];
  const phaseColors = ["#62aaa8", "#76b88d", "#d5a960", "#9b8ecb"];
  const zoomPluginAvailable = typeof window.ChartZoom !== "undefined";
  let selectedDayStartMs = 0;
  let requestGeneration = 0;
  let followsLatestDay = false;
  let timingSelectionLocked = false;
  let tooltipCanvas = null;
  let metricResizeFrame = 0;

  const cssColor = (element, name, fallback) =>
    getComputedStyle(element).getPropertyValue(name).trim() || fallback;

  const TOKYO_OFFSET_MS = 9 * 60 * 60 * 1000;
  const japanDateString = (timestamp) => new Date(timestamp + TOKYO_OFFSET_MS).toISOString().slice(0, 10);
  const tokyoDayStart = (timestamp) =>
    Math.floor((timestamp + TOKYO_OFFSET_MS) / DAY_MS) * DAY_MS - TOKYO_OFFSET_MS;

  const parseTokyoDate = (value) => {
    if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return null;
    const timestamp = Date.parse(value + "T00:00:00Z") - TOKYO_OFFSET_MS;
    return Number.isFinite(timestamp) && japanDateString(timestamp) === value ? timestamp : null;
  };

  const formatChartTimeParts = (timestamp) => {
    const date = new Date(Number(timestamp) + TOKYO_OFFSET_MS);
    const pad = (value) => String(value).padStart(2, "0");
    return {
      date: String(date.getUTCFullYear()).padStart(4, "0") + "-" +
        pad(date.getUTCMonth() + 1) + "-" + pad(date.getUTCDate()),
      time: pad(date.getUTCHours()) + ":" + pad(date.getUTCMinutes()) + ":" +
        pad(date.getUTCSeconds()),
    };
  };

  const formatChartTime = (timestamp) => {
    const parts = formatChartTimeParts(timestamp);
    return parts.date + " " + parts.time + " JST";
  };

  const formatChartValue = (value) => {
    const number = Number(value);
    if (!Number.isFinite(number)) return "—";
    if (number === 0) return "0";
    const prefixes = ["", "k", "M", "G", "T", "P", "E", "Z", "Y", "R", "Q"];
    const magnitude = Math.abs(number);
    const index = Math.min(prefixes.length - 1, Math.floor(Math.log10(magnitude) / 3));
    const scaled = number / (1000 ** index);
    return scaled.toLocaleString("en-US", { maximumFractionDigits: 2 }) + prefixes[index];
  };

  const formatMetricValue = (value) => {
    const number = Number(value);
    if (!Number.isFinite(number)) return "—";
    if (number === 0) return "0";
    if (Math.abs(number) >= 1000) return formatChartValue(number);
    return number.toLocaleString("en-US", { maximumFractionDigits: 6 });
  };

  const updateTimingPanel = (point) => {
    if (!timingPanel) return;
    const timing = point?.httpTiming || {};
    const selectedAt = timingPanel.querySelector("[data-http-timing-selected-at]");
    if (selectedAt) selectedAt.textContent = point ? formatChartTime(point.x) : "—";
    for (const name of ["dns", "tcp_connect", "tls_handshake", "request_to_headers", "total"]) {
      const value = timingPanel.querySelector('[data-http-phase-value="' + name + '"]');
      if (value) value.textContent = timing[name] || "—";
    }
  };

  const updateResponseStats = (chartData) => {
    const points = chartData.datasets[0]?.data || [];
    let count = 0;
    let total = 0;
    let maximum = null;
    for (const point of points) {
      if (!point.inRange || !Number.isFinite(point.y)) continue;
      count += 1;
      total += point.y;
      maximum = maximum === null ? point.y : Math.max(maximum, point.y);
    }
    const average = count ? total / count : null;
    const averageElement = page.querySelector('[data-response-stat="average"]');
    const maximumElement = page.querySelector('[data-response-stat="maximum"]');
    if (averageElement) averageElement.textContent = average === null ? "—" : formatChartValue(average) + " ms";
    if (maximumElement) maximumElement.textContent = maximum === null ? "—" : formatChartValue(maximum) + " ms";
  };

  const hideChartTooltip = () => {
    if (!pointTooltip) return;
    tooltipCanvas?.removeAttribute("aria-describedby");
    tooltipCanvas = null;
    pointTooltip.hidden = true;
    pointTooltip.replaceChildren();
    pointTooltip.classList.remove("metric-tooltip");
    delete pointTooltip.dataset.chartTooltipSnapshot;
  };

  const chartTooltipValue = (item, isMetric) => {
    const raw = item.raw || {};
    if (raw.displayValue) return raw.displayValue;
    if (!Number.isFinite(item.parsed?.y)) return "—";
    const unit = isMetric
      ? (item.dataset.unit ? " " + item.dataset.unit : "")
      : item.dataset.type === "bar" ? "" : " ms";
    return (isMetric ? formatMetricValue(item.parsed.y) : formatChartValue(item.parsed.y)) + unit;
  };

  const latestTimedPoint = (points) => {
    for (let index = points.length - 1; index >= 0; index -= 1) {
      const point = points[index];
      if (point.inRange && point.httpTiming) return point;
    }
    return null;
  };

  const renderChartTooltip = (context, isMetric) => {
    const { chart, tooltip } = context;
    const points = (tooltip.dataPoints || []).filter((item) =>
      !item.dataset.guide && item.raw?.inRange !== false);
    if (!pointTooltip || tooltip.opacity === 0 || !points.length) {
      hideChartTooltip();
      return;
    }

    const title = tooltip.title?.[0] || formatChartTime(points[0].parsed.x);
    const rows = points.map((item) => {
      let color = item.dataset.borderColor || item.dataset.backgroundColor;
      if (Array.isArray(color)) color = color[item.dataIndex];
      if (typeof color !== "string") color = cssColor(chart.canvas, "--accent", "#6ca8ff");
      return {
        key: item.dataset.key || item.dataset.label || "Metric",
        value: chartTooltipValue(item, isMetric),
        color,
      };
    });
    const snapshot = JSON.stringify([title, rows]);
    if (pointTooltip.dataset.chartTooltipSnapshot !== snapshot) {
      const time = document.createElement("div");
      time.className = "metric-tooltip-time";
      time.textContent = title;

      const values = document.createElement("div");
      values.className = "metric-tooltip-values";
      for (const rowData of rows) {
        const row = document.createElement("div");
        row.className = "metric-tooltip-row";
        row.style.setProperty("--metric-series-color", rowData.color);

        const key = document.createElement("span");
        key.className = "metric-tooltip-key";
        key.textContent = rowData.key;
        const value = document.createElement("strong");
        value.className = "metric-tooltip-value";
        value.textContent = rowData.value;
        row.append(key, value);
        values.append(row);
      }

      pointTooltip.replaceChildren(time, values);
      pointTooltip.dataset.chartTooltipSnapshot = snapshot;
    }

    if (tooltipCanvas !== chart.canvas) {
      tooltipCanvas?.removeAttribute("aria-describedby");
      tooltipCanvas = chart.canvas;
      tooltipCanvas.setAttribute("aria-describedby", "point-tooltip");
    }
    pointTooltip.classList.add("metric-tooltip");
    pointTooltip.hidden = false;

    const canvasBounds = chart.canvas.getBoundingClientRect();
    const anchorX = canvasBounds.left + tooltip.caretX;
    const anchorY = canvasBounds.top + tooltip.caretY;
    const bounds = pointTooltip.getBoundingClientRect();
    const gap = 12;
    const left = Math.max(8, Math.min(anchorX + gap, window.innerWidth - bounds.width - 8));
    let top = anchorY - bounds.height - gap;
    if (top < 8) top = anchorY + gap;
    top = Math.max(8, Math.min(top, window.innerHeight - bounds.height - 8));
    pointTooltip.style.left = `${left}px`;
    pointTooltip.style.top = `${top}px`;
  };

  const chartConfig = (canvas, chartData) => {
    const isMetric = chartData.kind === "metric";
    const hasPhaseDatasets = chartData.datasets.some((source) => source.kind === "bar");
    const accent = cssColor(canvas, "--accent", "#6ca8ff");
    const statusColors = {
      up: cssColor(canvas, "--up", "#55bb86"),
      slow: cssColor(canvas, "--slow", "#e2ad54"),
      down: cssColor(canvas, "--down", "#e16f72"),
      pending: cssColor(canvas, "--muted", "#89909b"),
    };
    const datasets = chartData.datasets.map((source) => {
      if (source.kind === "bar") {
        const color = phaseColors[source.colorIndex - 1] || accent;
        return {
          type: "bar",
          label: source.label,
          key: source.key,
          data: source.data,
          parsing: false,
          yAxisID: "phase",
          stack: "http-timing",
          backgroundColor: color,
          borderColor: color,
          borderWidth: 0,
          barThickness: "flex",
          maxBarThickness: 6,
          categoryPercentage: 0.8,
          barPercentage: 0.9,
          order: 2,
        };
      }

      const palette = metricPalette[source.colorIndex] || "--accent";
      const isResponseOverlay = source.kind === "overlay";
      const color = isMetric || isResponseOverlay
        ? (palette.startsWith("--") ? cssColor(canvas, palette, accent) : palette)
        : accent;
      return {
        type: "line",
        label: isMetric ? (source.key || source.label) : source.label,
        key: source.key,
        unit: source.unit,
        data: source.data,
        parsing: false,
        yAxisID: "y",
        borderColor: color,
        backgroundColor: color,
        borderWidth: 2,
        pointRadius: 0,
        pointHoverRadius: 4,
        pointHitRadius: 8,
        pointBackgroundColor: isMetric || isResponseOverlay
          ? color
          : (context) => statusColors[context.raw?.statusClass] || statusColors.pending,
        pointBorderColor: cssColor(canvas, "--surface", "#20242b"),
        pointBorderWidth: 1,
        tension: 0,
        spanGaps: false,
        order: 0,
      };
    });

    if (!isMetric && Number.isFinite(chartData.average)) {
      datasets.push({
        type: "line",
        label: "Average",
        key: "__average",
        guide: true,
        data: [
          { x: chartData.xMin, y: chartData.average },
          { x: chartData.xMax, y: chartData.average },
        ],
        parsing: false,
        yAxisID: "y",
        borderColor: cssColor(canvas, "--up", "#55bb86"),
        borderWidth: 1.5,
        borderDash: [4, 4],
        pointRadius: 0,
        pointHitRadius: 0,
        order: 1,
      });
    }
    if (!isMetric && Number.isFinite(chartData.threshold)) {
      datasets.push({
        type: "line",
        label: "Threshold",
        key: "__threshold",
        guide: true,
        data: [
          { x: chartData.xMin, y: chartData.threshold },
          { x: chartData.xMax, y: chartData.threshold },
        ],
        parsing: false,
        yAxisID: "y",
        borderColor: cssColor(canvas, "--slow", "#e2ad54"),
        borderWidth: 1.5,
        borderDash: [6, 5],
        pointRadius: 0,
        pointHitRadius: 0,
        order: 1,
      });
    }

    return {
      type: "line",
      data: { datasets },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        animation: false,
        parsing: false,
        font: {
          family: getComputedStyle(canvas).fontFamily || "sans-serif",
          size: 11,
        },
        interaction: { mode: "index", axis: "x", intersect: false },
        onHover: (_event, active, chart) => {
          if (isMetric) return;
          const point = active.length ? chart.data.datasets[0]?.data[active[0].index] : null;
          if (point?.inRange) {
            timingSelectionLocked = true;
            updateTimingPanel(point);
          }
        },
        onClick: (_event, active, chart) => {
          if (isMetric) return;
          const point = active.length ? chart.data.datasets[0]?.data[active[0].index] : null;
          if (point?.inRange) {
            timingSelectionLocked = true;
            updateTimingPanel(point);
          }
        },
        scales: {
          x: {
            type: "linear",
            min: chartData.xMin,
            max: chartData.xMax,
            offset: false,
            afterFit: (scale) => {
              scale.height = CHART_X_AXIS_HEIGHT;
            },
            grid: { color: cssColor(canvas, "--line", "#39404a") },
            ticks: {
              color: cssColor(canvas, "--muted", "#89909b"),
              autoSkip: true,
              maxTicksLimit: 8,
              maxRotation: 0,
              callback: (value) => formatChartTime(value),
            },
            border: { color: cssColor(canvas, "--muted", "#89909b") },
          },
          y: {
            type: "linear",
            min: chartData.yMin,
            max: chartData.yMax,
            afterFit: (scale) => {
              scale.width = CHART_Y_AXIS_WIDTH;
            },
            grid: { color: cssColor(canvas, "--line", "#39404a") },
            ticks: {
              color: cssColor(canvas, "--muted", "#89909b"),
              autoSkip: true,
              maxTicksLimit: 5,
              callback: (value) => formatChartValue(value),
            },
            border: { color: cssColor(canvas, "--muted", "#89909b") },
          },
          ...(!isMetric && hasPhaseDatasets ? {
            phase: {
              type: "linear",
              axis: "y",
              position: "left",
              min: chartData.yMin,
              max: chartData.yMax,
              display: false,
              afterFit: (scale) => {
                scale.width = 0;
              },
              stacked: true,
              grid: { display: false },
              ticks: { display: false },
              border: { display: false },
            },
          } : {}),
        },
        plugins: {
          legend: {
            display: true,
            position: "top",
            labels: {
              color: cssColor(canvas, "--muted", "#89909b"),
              usePointStyle: true,
              boxWidth: 8,
              boxHeight: 8,
              padding: 10,
              filter: (item, data) => {
                const dataset = data.datasets[item.datasetIndex];
                return Boolean(dataset?.guide) || dataset?.data?.some((point) => Number.isFinite(point?.y));
              },
            },
          },
          tooltip: {
            enabled: false,
            mode: "index",
            intersect: false,
            external: (context) => renderChartTooltip(context, isMetric),
            filter: (item) => !item.dataset.guide,
            callbacks: {
              title: (items) => items.length ? formatChartTime(items[0].parsed.x) : "",
            },
          },
          ...(zoomPluginAvailable ? {
            zoom: {
              limits: { x: { min: chartData.xMin, max: chartData.xMax, minRange: 10_000 } },
              pan: { enabled: true, mode: "x" },
              zoom: {
                mode: "x",
                wheel: { enabled: true, modifierKey: "ctrl" },
                pinch: { enabled: true },
              },
            },
          } : {}),
        },
      },
    };
  };

  const initializeChart = (canvas, chartData, key) => {
    const chart = new Chart(canvas.getContext("2d"), chartConfig(canvas, chartData));
    chartInstances.set(key, chart);
    return chart;
  };

  const updateMetricChartShellHeights = () => {
    if (!metricGroupsRoot) return;
    const measureContext = document.createElement("canvas").getContext("2d");
    if (!measureContext) return;

    for (const shell of metricGroupsRoot.querySelectorAll(".metric-chart-shell")) {
      const canvas = shell.querySelector("canvas.metric-chart");
      const chart = canvas ? chartInstances.get(canvas.dataset.chartKey) : null;
      const datasets = chart?.data.datasets || [];
      const fontFamily = getComputedStyle(shell).fontFamily || "sans-serif";
      measureContext.font = `11px ${fontFamily}`;

      const availableWidth = Math.max(1, shell.clientWidth - CHART_Y_AXIS_WIDTH);
      let rows = 1;
      let rowWidth = 0;
      for (const dataset of datasets) {
        const itemWidth = measureContext.measureText(dataset.label || "Metric").width + 32;
        if (rowWidth > 0 && rowWidth + itemWidth > availableWidth) {
          rows += 1;
          rowWidth = 0;
        }
        rowWidth += itemWidth;
      }

      shell.style.height = `${250 + (rows - 1) * 24}px`;
    }
  };

  const destroyCharts = () => {
    hideChartTooltip();
    for (const chart of chartInstances.values()) chart.destroy();
    chartInstances.clear();
  };

  const metricChartKey = (group, chart, index) => {
    const series = (chart.chartData?.datasets || []).map((dataset) => dataset.key || dataset.label).join("\u001f");
    return "metric:" + group.name + ":" + (series || chart.ariaLabel || String(index));
  };

  const createMetricGroup = (group) => {
    const groupElement = document.createElement("div");
    groupElement.className = "metric-group";
    const heading = document.createElement("h3");
    const name = document.createElement("span");
    name.className = "metric-group-name";
    name.textContent = group.name;
    heading.append(name);
    if (group.unit) {
      const unit = document.createElement("span");
      unit.className = "metric-group-unit";
      unit.textContent = group.unit;
      heading.append(unit);
    }
    groupElement.append(heading);

    for (const [index, chartData] of (group.charts || []).entries()) {
      const card = document.createElement("article");
      card.className = "metric-chart-card";
      const shell = document.createElement("div");
      shell.className = "metric-chart-shell";
      const canvas = document.createElement("canvas");
      canvas.className = "metric-chart";
      canvas.setAttribute("role", "img");
      canvas.setAttribute("aria-label", chartData.ariaLabel || group.name + " metrics chart");
      const key = metricChartKey(group, chartData, index);
      canvas.dataset.chartKey = key;
      shell.append(canvas);
      card.append(shell);
      groupElement.append(card);
      initializeChart(canvas, chartData.chartData, key);
    }
    return groupElement;
  };

  const updateChartInstance = (chart, canvas, chartData) => {
    const config = chartConfig(canvas, chartData);
    const visibility = new Map(chart.data.datasets.map((dataset, index) => [
      dataset.key || dataset.label,
      chart.isDatasetVisible(index),
    ]));
    const xScale = chart.scales.x;
    const wasZoomed = typeof chart.isZoomedOrPanned === "function"
      ? chart.isZoomedOrPanned()
      : typeof chart.getZoomLevel === "function" && chart.getZoomLevel() > 1;
    const zoomRange = wasZoomed && xScale ? { min: xScale.min, max: xScale.max } : null;

    chart.data.datasets = config.data.datasets;
    chart.options = config.options;
    for (const [index, dataset] of chart.data.datasets.entries()) {
      const key = dataset.key || dataset.label;
      if (visibility.has(key)) chart.setDatasetVisibility(index, visibility.get(key));
    }
    if (zoomRange) {
      chart.options.scales.x.min = zoomRange.min;
      chart.options.scales.x.max = zoomRange.max;
    }
    chart.update("none");
  };

  const destroyMetricCharts = () => {
    if (tooltipCanvas?.classList.contains("metric-chart")) hideChartTooltip();
    for (const [key, chart] of chartInstances) {
      if (!key.startsWith("metric:")) continue;
      chart.destroy();
      chartInstances.delete(key);
    }
  };

  const fetchChartRange = (startMs, endMs) => {
    const url = chartDataUrl + "/" + windowEndMs + "/" + startMs + "/" + endMs;
    if (!chartRangeRequests.has(url)) {
      const request = fetch(url, { headers: { Accept: "application/json" } }).then(async (response) => {
        if (!response.ok) throw new Error("Chart data request failed: " + response.status);
        return response.json();
      });
      chartRangeRequests.set(url, request);
      request.then(() => chartRangeRequests.delete(url), () => chartRangeRequests.delete(url));
    }
    return chartRangeRequests.get(url);
  };

  const visibleDayRange = (dayStartMs) => ({
    start: Math.max(dayStartMs, retentionStartMs),
    end: Math.min(dayStartMs + DAY_MS, windowEndMs),
  });

  const updateDayControls = () => {
    const earliestDay = tokyoDayStart(retentionStartMs);
    const latestDay = tokyoDayStart(windowEndMs - 1);
    const selectedDate = japanDateString(selectedDayStartMs);
    if (dateInput) {
      dateInput.value = selectedDate;
      dateInput.min = japanDateString(earliestDay);
      dateInput.max = japanDateString(latestDay);
    }
    if (previousDayButton) previousDayButton.disabled = selectedDayStartMs <= earliestDay;
    if (nextDayButton) nextDayButton.disabled = selectedDayStartMs >= latestDay;
  };

  const saveSelectedDayInUrl = () => {
    const url = new URL(window.location.href);
    url.searchParams.set("date", japanDateString(selectedDayStartMs));
    window.history.replaceState(null, "", url.pathname + url.search + url.hash);
  };

  const renderChartData = (payload) => {
    destroyCharts();
    timingSelectionLocked = false;
    responseShell.replaceChildren(responseCanvas);
    responseCanvas.setAttribute("aria-label", payload.responseChart.ariaLabel || "Response time chart");
    initializeChart(responseCanvas, payload.responseChart.chartData, "response");
    updateResponseStats(payload.responseChart.chartData);

    const responsePoints = payload.responseChart.chartData.datasets[0]?.data || [];
    const latestPoint = latestTimedPoint(responsePoints);
    if (!timingSelectionLocked) updateTimingPanel(latestPoint || null);

    const responseEmptyNotice = document.createElement("div");
    responseEmptyNotice.className = "response-chart-empty";
    responseEmptyNotice.textContent = "No data yet.";
    responseEmptyNotice.hidden = (payload.responseChart.dataPointCount || 0) > 0;
    responseShell.append(responseEmptyNotice);

    if (metricGroupsRoot) {
      metricGroupsRoot.replaceChildren();
      for (const group of payload.metricGroups || []) metricGroupsRoot.append(createMetricGroup(group));
      metricPanel.hidden = metricGroupsRoot.children.length === 0;
      updateMetricChartShellHeights();
    }
    if (resetZoomButton) resetZoomButton.disabled = !zoomPluginAvailable;
    if (loadError) loadError.hidden = true;
  };

  const refreshChartData = (payload) => {
    const responseChart = chartInstances.get("response");
    if (!responseChart || !metricGroupsRoot) {
      renderChartData(payload);
      return;
    }

    responseCanvas.setAttribute("aria-label", payload.responseChart.ariaLabel || "Response time chart");
    updateChartInstance(responseChart, responseCanvas, payload.responseChart.chartData);
    updateResponseStats(payload.responseChart.chartData);
    const responsePoints = payload.responseChart.chartData.datasets[0]?.data || [];
    const latestPoint = latestTimedPoint(responsePoints);
    if (!timingSelectionLocked) updateTimingPanel(latestPoint || null);

    const emptyNotice = responseShell.querySelector(".response-chart-empty");
    if (emptyNotice) emptyNotice.hidden = (payload.responseChart.dataPointCount || 0) > 0;

    const nextMetricCharts = (payload.metricGroups || []).flatMap((group) =>
      (group.charts || []).map((chart, index) => ({ key: metricChartKey(group, chart, index), chart })));
    const currentMetricKeys = Array.from(chartInstances.keys()).filter((key) => key.startsWith("metric:")).sort();
    const nextMetricKeys = nextMetricCharts.map((item) => item.key).sort();
    if (JSON.stringify(currentMetricKeys) !== JSON.stringify(nextMetricKeys)) {
      destroyMetricCharts();
      metricGroupsRoot.replaceChildren();
      for (const group of payload.metricGroups || []) metricGroupsRoot.append(createMetricGroup(group));
    } else {
      for (const item of nextMetricCharts) {
        const chart = chartInstances.get(item.key);
        if (chart) updateChartInstance(chart, chart.canvas, item.chart.chartData);
      }
    }
    metricPanel.hidden = metricGroupsRoot.children.length === 0;
    updateMetricChartShellHeights();
    if (resetZoomButton) resetZoomButton.disabled = !zoomPluginAvailable;
    if (loadError) loadError.hidden = true;
  };

  const clearCharts = () => {
    destroyCharts();
    responseShell.replaceChildren(responseCanvas);
    if (metricGroupsRoot) metricGroupsRoot.replaceChildren();
    if (metricPanel) metricPanel.hidden = true;
    timingSelectionLocked = false;
    updateResponseStats({ datasets: [] });
    updateTimingPanel(null);
  };

  const loadSelectedDay = async () => {
    const generation = ++requestGeneration;
    const { start, end } = visibleDayRange(selectedDayStartMs);
    if (start >= end) return;
    clearCharts();
    if (loadError) {
      loadError.textContent = "Loading…";
      loadError.hidden = false;
    }
    try {
      const payload = await fetchChartRange(start, end);
      if (generation !== requestGeneration) return;
      renderChartData(payload);
    } catch (error) {
      if (generation !== requestGeneration) return;
      console.warn("Could not load chart data", error);
      if (loadError) {
        loadError.textContent = "Could not load chart data.";
        loadError.hidden = false;
      }
    }
  };

  const selectDay = (dayStartMs) => {
    const earliestDay = tokyoDayStart(retentionStartMs);
    const latestDay = tokyoDayStart(windowEndMs - 1);
    selectedDayStartMs = Math.max(earliestDay, Math.min(latestDay, dayStartMs));
    followsLatestDay = selectedDayStartMs === latestDay;
    updateDayControls();
    saveSelectedDayInUrl();
    void loadSelectedDay();
  };

  const applyLiveWindow = async (detail) => {
    const nextWindowEndMs = Number(detail?.chartWindowEndMs);
    const nextRetentionStartMs = Number(detail?.chartRetentionStartMs);
    if (!Number.isFinite(nextWindowEndMs) || !Number.isFinite(nextRetentionStartMs) || nextWindowEndMs <= nextRetentionStartMs) return;

    const wasFollowingLatest = followsLatestDay;
    windowEndMs = nextWindowEndMs;
    retentionStartMs = nextRetentionStartMs;
    page.dataset.windowEndMs = String(windowEndMs);
    page.dataset.retentionStartMs = String(retentionStartMs);
    const earliestDay = tokyoDayStart(retentionStartMs);
    const latestDay = tokyoDayStart(windowEndMs - 1);
    if (wasFollowingLatest) selectedDayStartMs = latestDay;
    else selectedDayStartMs = Math.max(earliestDay, Math.min(latestDay, selectedDayStartMs));
    followsLatestDay = wasFollowingLatest;
    updateDayControls();
    if (!followsLatestDay || selectedDayStartMs !== latestDay) return;

    const { start, end } = visibleDayRange(selectedDayStartMs);
    if (start >= end) return;
    const generation = ++requestGeneration;
    try {
      const payload = await fetchChartRange(start, end);
      if (generation !== requestGeneration) return;
      refreshChartData(payload);
    } catch (error) {
      if (generation !== requestGeneration) return;
      console.warn("Could not refresh chart data", error);
    }
  };

  const initializePage = () => {
    if (!Number.isFinite(retentionStartMs) || !Number.isFinite(windowEndMs) || windowEndMs <= retentionStartMs) return;
    const earliestDay = tokyoDayStart(retentionStartMs);
    const latestDay = tokyoDayStart(windowEndMs - 1);
    const queryDate = new URLSearchParams(window.location.search).get("date");
    const queryDay = queryDate ? parseTokyoDate(queryDate) : null;
    selectedDayStartMs = Math.max(earliestDay, Math.min(latestDay, queryDay ?? latestDay));
    followsLatestDay = !queryDay || selectedDayStartMs === latestDay;
    updateDayControls();
    if (queryDate && queryDate !== japanDateString(selectedDayStartMs)) saveSelectedDayInUrl();

    dateInput?.addEventListener("change", () => {
      const selected = parseTokyoDate(dateInput.value);
      if (selected !== null) selectDay(selected);
    });
    previousDayButton?.addEventListener("click", () => selectDay(selectedDayStartMs - DAY_MS));
    nextDayButton?.addEventListener("click", () => selectDay(selectedDayStartMs + DAY_MS));
    resetZoomButton?.addEventListener("click", () => {
      for (const chart of chartInstances.values()) chart.resetZoom?.();
    });
    window.addEventListener("status:update", (event) => { void applyLiveWindow(event.detail); });
    window.addEventListener("resize", () => {
      if (metricResizeFrame) cancelAnimationFrame(metricResizeFrame);
      metricResizeFrame = requestAnimationFrame(() => {
        metricResizeFrame = 0;
        updateMetricChartShellHeights();
      });
    });
    void loadSelectedDay();
  };

  initializePage();
})();
