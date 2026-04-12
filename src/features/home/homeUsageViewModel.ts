import type {
  AccountSnapshot,
  LocalUsageDay,
  LocalUsageSnapshot,
  RateLimitSnapshot,
} from "../../types";
import { formatRelativeTime } from "../../utils/time";
import { getUsageLabels } from "../app/utils/usageLabels";
import {
  buildWindowCaption,
  formatAccountTypeLabel,
  formatCompactNumber,
  formatCount,
  formatCreditsBalance,
  formatDayCount,
  formatDayLabel,
  formatDuration,
  formatDurationCompact,
  formatPlanType,
  isUsageDayActive,
} from "./homeFormatters";
import type { HomeStatCard, UsageMetric } from "./homeTypes";

type TFunc = (key: string, options?: Record<string, unknown>) => string;

type HomeUsageViewModel = {
  accountCards: HomeStatCard[];
  accountMeta: string | null;
  updatedLabel: string | null;
  usageCards: HomeStatCard[];
  usageDays: LocalUsageDay[];
  usageInsights: HomeStatCard[];
};

export function buildHomeUsageViewModel({
  accountInfo,
  accountRateLimits,
  localUsageSnapshot,
  usageMetric,
  usageShowRemaining,
  t,
}: {
  accountInfo: AccountSnapshot | null;
  accountRateLimits: RateLimitSnapshot | null;
  localUsageSnapshot: LocalUsageSnapshot | null;
  usageMetric: UsageMetric;
  usageShowRemaining: boolean;
  t: TFunc;
}): HomeUsageViewModel {
  const usageTotals = localUsageSnapshot?.totals ?? null;
  const usageDays = localUsageSnapshot?.days ?? [];
  const latestUsageDay = usageDays[usageDays.length - 1] ?? null;
  const last7Days = usageDays.slice(-7);
  const last7Tokens = last7Days.reduce((total, day) => total + day.totalTokens, 0);
  const last7Input = last7Days.reduce((total, day) => total + day.inputTokens, 0);
  const last7Cached = last7Days.reduce(
    (total, day) => total + day.cachedInputTokens,
    0,
  );
  const last7AgentMs = last7Days.reduce(
    (total, day) => total + (day.agentTimeMs ?? 0),
    0,
  );
  const last30AgentMs = usageDays.reduce(
    (total, day) => total + (day.agentTimeMs ?? 0),
    0,
  );
  const averageDailyAgentMs =
    last7Days.length > 0 ? Math.round(last7AgentMs / last7Days.length) : 0;
  const last7AgentRuns = last7Days.reduce(
    (total, day) => total + (day.agentRuns ?? 0),
    0,
  );
  const last30AgentRuns = usageDays.reduce(
    (total, day) => total + (day.agentRuns ?? 0),
    0,
  );
  const averageTokensPerRun =
    last7AgentRuns > 0 ? Math.round(last7Tokens / last7AgentRuns) : null;
  const averageRunDurationMs =
    last7AgentRuns > 0 ? Math.round(last7AgentMs / last7AgentRuns) : null;
  const last7ActiveDays = last7Days.filter(isUsageDayActive).length;
  const last30ActiveDays = usageDays.filter(isUsageDayActive).length;
  const averageActiveDayAgentMs =
    last7ActiveDays > 0 ? Math.round(last7AgentMs / last7ActiveDays) : null;
  const peakAgentDay = usageDays.reduce<
    | { day: string; agentTimeMs: number }
    | null
  >((best, day) => {
    const value = day.agentTimeMs ?? 0;
    if (value <= 0) {
      return best;
    }
    if (!best || value > best.agentTimeMs) {
      return { day: day.day, agentTimeMs: value };
    }
    return best;
  }, null);

  let longestStreak = 0;
  let runningStreak = 0;
  for (const day of usageDays) {
    if (isUsageDayActive(day)) {
      runningStreak += 1;
      longestStreak = Math.max(longestStreak, runningStreak);
    } else {
      runningStreak = 0;
    }
  }

  const usageCards: HomeStatCard[] =
    usageMetric === "tokens"
      ? [
          {
            label: t("home.usage.cardToday"),
            value: formatCompactNumber(latestUsageDay?.totalTokens ?? 0),
            suffix: t("home.usage.suffixTokens"),
            caption: latestUsageDay
              ? `${formatDayLabel(latestUsageDay.day)} · ${formatCount(
                  latestUsageDay.inputTokens,
                )} in / ${formatCount(latestUsageDay.outputTokens)} out`
              : t("home.usage.captionLatestDay"),
          },
          {
            label: t("home.usage.cardLast7"),
            value: formatCompactNumber(usageTotals?.last7DaysTokens ?? last7Tokens),
            suffix: t("home.usage.suffixTokens"),
            caption: t("home.usage.captionAvgPerDay", {
              value: formatCompactNumber(usageTotals?.averageDailyTokens),
            }),
          },
          {
            label: t("home.usage.cardLast30"),
            value: formatCompactNumber(usageTotals?.last30DaysTokens ?? last7Tokens),
            suffix: t("home.usage.suffixTokens"),
            caption: t("home.usage.captionTotal", {
              value: formatCount(usageTotals?.last30DaysTokens ?? last7Tokens),
            }),
          },
          {
            label: t("home.usage.cardCacheHitRate"),
            value: usageTotals
              ? `${usageTotals.cacheHitRatePercent.toFixed(1)}%`
              : "--",
            caption: t("home.usage.captionLast7Days"),
          },
          {
            label: t("home.usage.cardCachedTokens"),
            value: formatCompactNumber(last7Cached),
            suffix: t("home.usage.suffixSaved"),
            caption:
              last7Input > 0
                ? t("home.usage.captionCachePercent", {
                    percent: ((last7Cached / last7Input) * 100).toFixed(1),
                  })
                : t("home.usage.captionLast7Days"),
          },
          {
            label: t("home.usage.cardAvgPerRun"),
            value:
              averageTokensPerRun === null
                ? "--"
                : formatCompactNumber(averageTokensPerRun),
            suffix: t("home.usage.suffixTokens"),
            caption:
              last7AgentRuns > 0
                ? t("home.usage.captionRunsLast7", { count: formatCount(last7AgentRuns) })
                : t("home.usage.captionNoRuns"),
          },
          {
            label: t("home.usage.cardPeakDay"),
            value: formatDayLabel(usageTotals?.peakDay),
            caption: t("home.usage.captionTotal", {
              value: `${formatCompactNumber(usageTotals?.peakDayTokens)} ${t("home.usage.suffixTokens")}`,
            }),
          },
        ]
      : [
          {
            label: t("home.usage.cardLast7"),
            value: formatDurationCompact(last7AgentMs),
            suffix: t("home.usage.suffixAgentTime"),
            caption: t("home.usage.captionAvgPerDay", {
              value: formatDurationCompact(averageDailyAgentMs),
            }),
          },
          {
            label: t("home.usage.cardLast30"),
            value: formatDurationCompact(last30AgentMs),
            suffix: t("home.usage.suffixAgentTime"),
            caption: t("home.usage.captionTotal", {
              value: formatDuration(last30AgentMs),
            }),
          },
          {
            label: t("home.usage.cardRuns"),
            value: formatCount(last7AgentRuns),
            suffix: t("home.usage.suffixRuns"),
            caption: `Last 30 days: ${formatCount(last30AgentRuns)} runs`,
          },
          {
            label: t("home.usage.cardAvgPerRun"),
            value: formatDurationCompact(averageRunDurationMs),
            caption:
              last7AgentRuns > 0
                ? t("home.usage.captionAcrossRuns", { count: formatCount(last7AgentRuns) })
                : t("home.usage.captionNoRuns"),
          },
          {
            label: t("home.usage.cardAvgPerActiveDay"),
            value: formatDurationCompact(averageActiveDayAgentMs),
            caption:
              last7ActiveDays > 0
                ? t("home.usage.captionActiveDaysLast7", { active: formatCount(last7ActiveDays) })
                : t("home.usage.captionNoActiveDays"),
          },
          {
            label: t("home.usage.cardPeakDay"),
            value: formatDayLabel(peakAgentDay?.day ?? null),
            caption: `${formatDurationCompact(peakAgentDay?.agentTimeMs ?? 0)} ${t("home.usage.suffixAgentTime")}`,
          },
        ];

  const usageInsights = [
    {
      label: t("home.usage.cardLongestStreak"),
      value: longestStreak > 0 ? formatDayCount(longestStreak) : "--",
      caption:
        longestStreak > 0
          ? t("home.usage.captionLongestStreak")
          : t("home.usage.captionNoStreak"),
      compact: true,
    },
    {
      label: t("home.usage.cardActiveDays"),
      value: last7Days.length > 0 ? `${last7ActiveDays} / ${last7Days.length}` : "--",
      caption:
        usageDays.length > 0
          ? t("home.usage.captionActiveDaysRange", {
              active: last30ActiveDays,
              total: usageDays.length,
            })
          : t("home.usage.captionNoActivity"),
      compact: true,
    },
  ] satisfies HomeStatCard[];

  const usagePercentLabels = getUsageLabels(accountRateLimits, usageShowRemaining);
  const planLabel = formatPlanType(accountRateLimits?.planType ?? accountInfo?.planType);
  const creditsBalance = formatCreditsBalance(accountRateLimits?.credits?.balance);
  const accountCards: HomeStatCard[] = [];

  if (usagePercentLabels.sessionPercent !== null) {
    accountCards.push({
      label: usageShowRemaining
        ? t("home.usage.cardSessionLeft")
        : t("home.usage.cardSessionUsage"),
      value: `${usagePercentLabels.sessionPercent}%`,
      caption: buildWindowCaption(
        usagePercentLabels.sessionResetLabel,
        accountRateLimits?.primary?.windowDurationMins,
        t("home.usage.captionCurrentWindow"),
      ),
    });
  }

  if (usagePercentLabels.showWeekly && usagePercentLabels.weeklyPercent !== null) {
    accountCards.push({
      label: usageShowRemaining
        ? t("home.usage.cardWeeklyLeft")
        : t("home.usage.cardWeeklyUsage"),
      value: `${usagePercentLabels.weeklyPercent}%`,
      caption: buildWindowCaption(
        usagePercentLabels.weeklyResetLabel,
        accountRateLimits?.secondary?.windowDurationMins,
        t("home.usage.captionLongerWindow"),
      ),
    });
  }

  if (accountRateLimits?.credits?.hasCredits) {
    accountCards.push(
      accountRateLimits.credits.unlimited
        ? {
            label: t("home.usage.cardCredits"),
            value: t("home.usage.cardCreditsUnlimited"),
            caption: t("home.usage.captionAvailableBalance"),
          }
        : {
            label: t("home.usage.cardCredits"),
            value: creditsBalance ?? "--",
            suffix: creditsBalance ? t("home.usage.cardCredits").toLowerCase() : null,
            caption: t("home.usage.captionAvailableBalance"),
          },
    );
  }

  if (planLabel) {
    accountCards.push({
      label: t("home.usage.cardPlan"),
      value: planLabel,
      caption: formatAccountTypeLabel(accountInfo?.type),
    });
  }

  return {
    accountCards,
    accountMeta: accountInfo?.email ?? null,
    updatedLabel: localUsageSnapshot
      ? `Updated ${formatRelativeTime(localUsageSnapshot.updatedAt)}`
      : null,
    usageCards,
    usageDays,
    usageInsights,
  };
}
