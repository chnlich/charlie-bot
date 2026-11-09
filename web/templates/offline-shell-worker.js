const WORKER_CONFIG = {{ worker_config_json|safe }};

{% include 'js/offline-shell-worker.js' %}
