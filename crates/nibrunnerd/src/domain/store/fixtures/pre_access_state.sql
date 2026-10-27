-- Stored JSON shape from main before per-app access policies (8eb923f).
insert into host_identity values (0, 'host-1');
insert into slots values ('app-1', 7), ('app-2', 9);
insert into slot_cursor values (0, 11);
insert into instances (app_id, record) values ('app-1', '{
  "appId": "app-1",
  "deploymentId": "dep-1",
  "volumeId": "vol-1",
  "hostnames": [
    {
      "hostname": "app-1.apps.example.com",
      "kind": "platform"
    }
  ],
  "hostPort": 20001,
  "httpPort": 3000,
  "ports": [
    {
      "name": "ssh",
      "hostPort": 30001,
      "guestPort": 22
    }
  ],
  "guestIpv4": "10.80.1.2",
  "layerDigests": [
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  ],
  "state": "running",
  "health": {
    "consecutiveSuccesses": 4,
    "consecutiveFailures": 0,
    "everHealthy": true,
    "lastHealthyAt": "2026-10-26T12:00:00.000Z"
  },
  "healthCheck": {
    "kind": "http",
    "path": "/healthz",
    "intervalMs": 5000,
    "timeoutMs": 2000,
    "gracePeriodMs": 30000,
    "healthyThreshold": 1,
    "unhealthyThreshold": 3
  },
  "resources": {
    "vcpuCount": 1,
    "memoryMib": 256
  },
  "restartPolicy": {
    "maxRestarts": 5,
    "initialBackoffMs": 500,
    "maxBackoffMs": 30000,
    "backoffFactor": 2.0,
    "resetAfterMs": 60000
  },
  "desiredRunning": true,
  "onRequest": false,
  "startAttempts": {
    "attempts": 2,
    "lastAttemptAtMs": 1793010000000
  },
  "restartCount": 3,
  "stopRequested": false,
  "startedAt": "2026-10-26T11:00:00.000Z",
  "convergedAt": "2026-10-26T11:00:01.000Z",
  "lastExitCode": 137,
  "message": "retained across upgrade"
}');
insert into instances (app_id, record) values ('app-2', '{
  "appId": "app-2",
  "deploymentId": "dep-2",
  "volumeId": "vol-2",
  "hostnames": [
    {
      "hostname": "app-2.apps.example.com",
      "kind": "platform"
    }
  ],
  "hostPort": 20002,
  "httpPort": 3000,
  "ports": [
    {
      "name": "ssh",
      "hostPort": 30002,
      "guestPort": 22
    }
  ],
  "guestIpv4": "10.80.2.2",
  "layerDigests": [
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  ],
  "state": "idle",
  "health": {
    "consecutiveSuccesses": 4,
    "consecutiveFailures": 0,
    "everHealthy": true,
    "lastHealthyAt": "2026-10-26T12:00:00.000Z"
  },
  "healthCheck": {
    "kind": "http",
    "path": "/healthz",
    "intervalMs": 5000,
    "timeoutMs": 2000,
    "gracePeriodMs": 30000,
    "healthyThreshold": 1,
    "unhealthyThreshold": 3
  },
  "resources": {
    "vcpuCount": 1,
    "memoryMib": 256
  },
  "restartPolicy": {
    "maxRestarts": 5,
    "initialBackoffMs": 500,
    "maxBackoffMs": 30000,
    "backoffFactor": 2.0,
    "resetAfterMs": 60000
  },
  "desiredRunning": true,
  "onRequest": true,
  "startAttempts": {
    "attempts": 2,
    "lastAttemptAtMs": 1793010000000
  },
  "restartCount": 3,
  "stopRequested": false,
  "startedAt": "2026-10-26T11:00:00.000Z",
  "convergedAt": "2026-10-26T11:00:01.000Z",
  "lastExitCode": 137,
  "message": "retained across upgrade"
}');
insert into accepted_document values (0, '{
  "hostId": "host-1",
  "revision": "deploy-4821",
  "volumes": [
    {
      "volumeId": "vol-1",
      "appId": "app-1",
      "sizeBytes": 10485760,
      "desiredState": "present"
    },
    {
      "volumeId": "vol-2",
      "appId": "app-2",
      "sizeBytes": 10485760,
      "desiredState": "present"
    }
  ],
  "instances": [
    {
      "appId": "app-1",
      "deploymentId": "dep-1",
      "volumeId": "vol-1",
      "desiredState": "running",
      "layers": [
        {
          "kind": "filesystem",
          "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "objectKey": "layers/base"
        }
      ],
      "config": {
        "httpPort": 3000,
        "ports": [
          {
            "name": "ssh",
            "guestPort": 22
          }
        ],
        "command": {
          "program": "/app/server",
          "args": [
            "serve"
          ],
          "workingDirectory": "/app",
          "environment": {
            "PORT_HINT": "${NIBRUN_HTTP_PORT}"
          }
        },
        "resources": {
          "vcpuCount": 1,
          "memoryMib": 256
        },
        "healthCheck": {
          "kind": "http",
          "path": "/healthz",
          "intervalMs": 5000,
          "timeoutMs": 2000,
          "gracePeriodMs": 30000,
          "healthyThreshold": 1,
          "unhealthyThreshold": 3
        },
        "restartPolicy": {
          "maxRestarts": 5,
          "initialBackoffMs": 500,
          "maxBackoffMs": 30000,
          "backoffFactor": 2.0,
          "resetAfterMs": 60000
        }
      },
      "hostnames": [
        {
          "hostname": "app-1.apps.example.com",
          "kind": "platform"
        }
      ]
    },
    {
      "appId": "app-2",
      "deploymentId": "dep-2",
      "volumeId": "vol-2",
      "desiredState": "on-request",
      "layers": [
        {
          "kind": "filesystem",
          "digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "objectKey": "layers/base"
        }
      ],
      "config": {
        "httpPort": 3000,
        "ports": [
          {
            "name": "ssh",
            "guestPort": 22
          }
        ],
        "command": {
          "program": "/app/server",
          "args": [
            "serve"
          ],
          "workingDirectory": "/app",
          "environment": {
            "PORT_HINT": "${NIBRUN_HTTP_PORT}"
          }
        },
        "resources": {
          "vcpuCount": 1,
          "memoryMib": 256
        },
        "healthCheck": {
          "kind": "http",
          "path": "/healthz",
          "intervalMs": 5000,
          "timeoutMs": 2000,
          "gracePeriodMs": 30000,
          "healthyThreshold": 1,
          "unhealthyThreshold": 3
        },
        "restartPolicy": {
          "maxRestarts": 5,
          "initialBackoffMs": 500,
          "maxBackoffMs": 30000,
          "backoffFactor": 2.0,
          "resetAfterMs": 60000
        }
      },
      "hostnames": [
        {
          "hostname": "app-2.apps.example.com",
          "kind": "platform"
        }
      ]
    }
  ],
  "checkpoints": [],
  "exports": []
}', '6300aa3534d0f2425f61bc4c7a80a0e6f98edac7f793ff19ef92bd2b683f7f62');
insert into activity values ('app-1', 1793010000000), ('app-2', 1793010000123);
insert into meters values ('app-1', 100, 200, 300, 400, 500, 600, 700), ('app-2', 11, 22, 33, 44, 55, 66, 77);
insert into deleted_volumes values ('vol-deleted', '{"volumeId":"vol-deleted","appId":"app-deleted","state":"deleted","sizeBytes":0}');
insert into cron_tables values ('app-1', 'dep-1', '* * * * * /app/tick');
insert into cron_jobs values ('app-1', 0, '* * * * *', '/app/tick', 1);
insert into cron_job_environment values ('app-1', 0, 'MODE', 'scheduled');
