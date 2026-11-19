(() => {
  const table = window.CB_ICON_TABLE;

  window.cbIcon = function cbIcon(key, size, cls = "", attrs = "") {
    if (!Object.prototype.hasOwnProperty.call(table, key)) {
      throw new Error(`Unknown icon key: ${key}`);
    }

    const icon = table[key];
    if (!(size > 0)) {
      throw new Error(`icon '${key}' needs a positive pixel size, got ${size}`);
    }
    const className = cls === "" ? "cb-icon" : `cb-icon ${cls}`;
    const fill = icon.fill === true ? "currentColor" : "none";
    const strokeWidth = Math.max(2, Math.floor(24 / size));
    const extraAttributes = attrs === "" ? "" : ` ${attrs}`;

    return `<svg class="${className}" width="${size}" height="${size}" viewBox="0 0 24 24" fill="${fill}" stroke="currentColor" stroke-width="${strokeWidth}" stroke-linecap="round" stroke-linejoin="round"${extraAttributes}>${icon.shapes}</svg>`;
  };
})();
