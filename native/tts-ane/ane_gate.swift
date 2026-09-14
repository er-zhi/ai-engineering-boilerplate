import CoreML
import Foundation

/// Fails closed unless the compute plan prefers ANE for the required operations.
/// This is placement evidence, not a trace proving actual hardware execution.
///
/// `MLModelConfiguration.computeUnits = .cpuAndNeuralEngine` forbids the GPU,
/// but Core ML can still fall back to CPU for an unsupported graph.  Therefore
/// the compute plan is the admission gate, not a diagnostic printed after the
/// service has started.
@main
struct ANEGate {
    static let defaultMinimumANEOperationRatio = 1.0

    static func main() async {
        do {
            let arguments = try Arguments(CommandLine.arguments.dropFirst())
            let compiledURL = try await compileIfNeeded(arguments.modelURL)

            let configuration = MLModelConfiguration()
            configuration.computeUnits = .cpuAndNeuralEngine
            if arguments.fastPrediction {
                configuration.optimizationHints.specializationStrategy = .fastPrediction
            }

            let plan = try await MLComputePlan.load(
                contentsOf: compiledURL,
                configuration: configuration
            )
            let summary = try summarize(plan)

            guard summary.totalOperations > 0 else {
                throw GateError.unsupportedModelStructure
            }
            guard summary.gpuPreferredOperations == 0 else {
                throw GateError.gpuSelected(summary.gpuPreferredOperations)
            }

            let ratio = Double(summary.neuralEnginePreferredOperations)
                / Double(summary.totalOperations)
            guard ratio >= arguments.minimumANEOperationRatio else {
                throw GateError.insufficientANE(
                    actual: ratio,
                    required: arguments.minimumANEOperationRatio,
                    cpuPreferred: summary.cpuPreferredOperations
                )
            }

            let result: [String: Any] = [
                "status": "PASS",
                "fast_prediction": arguments.fastPrediction,
                "model": arguments.modelURL.path,
                "compiled_model": compiledURL.path,
                "minimum_ane_operation_ratio": arguments.minimumANEOperationRatio,
                "ane_operation_ratio": ratio,
                "total_operations": summary.totalOperations,
                "neural_engine_preferred_operations": summary.neuralEnginePreferredOperations,
                "cpu_preferred_operations": summary.cpuPreferredOperations,
                "cpu_preferred_operation_types": summary.cpuPreferredOperationTypes,
                "gpu_preferred_operations": summary.gpuPreferredOperations,
                "estimated_cost_by_operation_type": summary.estimatedCostByOperationType,
                "operations_with_estimated_cost": summary.operationsWithEstimatedCost,
            ]
            let data = try JSONSerialization.data(
                withJSONObject: result,
                options: [.prettyPrinted, .sortedKeys]
            )
            print(String(decoding: data, as: UTF8.self))
        } catch {
            fputs("ANE gate failed: \(error.localizedDescription)\n", stderr)
            exit(1)
        }
    }

    private static func compileIfNeeded(_ modelURL: URL) async throws -> URL {
        if modelURL.pathExtension == "mlmodelc" {
            return modelURL
        }
        guard modelURL.pathExtension == "mlpackage" || modelURL.pathExtension == "mlmodel" else {
            throw GateError.unsupportedModelPath(modelURL.path)
        }
        return try await MLModel.compileModel(at: modelURL)
    }

    private static func summarize(_ plan: MLComputePlan) throws -> OperationSummary {
        switch plan.modelStructure {
        case .program(let program):
            var summary = OperationSummary()
            for function in program.functions.values {
                count(function.block, plan: plan, into: &summary)
            }
            return summary
        case .neuralNetwork(let network):
            var summary = OperationSummary()
            for layer in network.layers {
                guard let usage = plan.deviceUsage(for: layer) else { continue }
                summary.record(usage.preferred, operationName: layer.name)
            }
            return summary
        case .pipeline, .unsupported:
            throw GateError.unsupportedModelStructure
        @unknown default:
            throw GateError.unsupportedModelStructure
        }
    }

    private static func count(
        _ block: MLModelStructure.Program.Block,
        plan: MLComputePlan,
        into summary: inout OperationSummary
    ) {
        for operation in block.operations {
            if let cost = plan.estimatedCost(of: operation) {
                summary.estimatedCostByOperationType[operation.operatorName, default: 0] += cost.weight
                summary.operationsWithEstimatedCost += 1
            }
            if let usage = plan.deviceUsage(for: operation) {
                summary.record(usage.preferred, operationName: operation.operatorName)
            }
            for nestedBlock in operation.blocks {
                count(nestedBlock, plan: plan, into: &summary)
            }
        }
    }
}

private struct OperationSummary {
    var totalOperations = 0
    var neuralEnginePreferredOperations = 0
    var cpuPreferredOperations = 0
    var gpuPreferredOperations = 0
    var cpuPreferredOperationTypes: [String: Int] = [:]
    // Relative model-estimated weights, not measured wall-clock milliseconds.
    var estimatedCostByOperationType: [String: Double] = [:]
    var operationsWithEstimatedCost = 0

    mutating func record(_ device: MLComputeDevice, operationName: String) {
        totalOperations += 1
        switch device {
        case .neuralEngine:
            neuralEnginePreferredOperations += 1
        case .cpu:
            cpuPreferredOperations += 1
            cpuPreferredOperationTypes[operationName, default: 0] += 1
        case .gpu:
            gpuPreferredOperations += 1
        @unknown default:
            cpuPreferredOperations += 1
            cpuPreferredOperationTypes[operationName, default: 0] += 1
        }
    }
}

private struct Arguments {
    let modelURL: URL
    let minimumANEOperationRatio: Double
    let fastPrediction: Bool

    init(_ rawArguments: ArraySlice<String>) throws {
        var arguments = Array(rawArguments)
        var minimumANEOperationRatio = ANEGate.defaultMinimumANEOperationRatio
        if let index = arguments.firstIndex(of: "--fast-prediction") {
            arguments.remove(at: index)
            self.fastPrediction = true
        } else {
            self.fastPrediction = false
        }

        if let index = arguments.firstIndex(of: "--min-ane-operation-ratio") {
            guard arguments.indices.contains(index + 1),
                  let value = Double(arguments[index + 1]),
                  (0...1).contains(value)
            else {
                throw GateError.invalidRatio
            }
            minimumANEOperationRatio = value
            arguments.removeSubrange(index...(index + 1))
        }

        guard arguments.count == 1 else {
            throw GateError.usage
        }
        self.modelURL = URL(fileURLWithPath: arguments[0]).standardizedFileURL
        self.minimumANEOperationRatio = minimumANEOperationRatio
    }
}

private enum GateError: LocalizedError {
    case usage
    case invalidRatio
    case unsupportedModelPath(String)
    case unsupportedModelStructure
    case gpuSelected(Int)
    case insufficientANE(actual: Double, required: Double, cpuPreferred: Int)

    var errorDescription: String? {
        switch self {
        case .usage:
            return "usage: ane-gate [--fast-prediction] [--min-ane-operation-ratio 0...1] model.mlpackage|model.mlmodelc"
        case .invalidRatio:
            return "--min-ane-operation-ratio must be a number in [0, 1]"
        case .unsupportedModelPath(let path):
            return "expected .mlpackage, .mlmodel, or .mlmodelc; got \(path)"
        case .unsupportedModelStructure:
            return "the model is not a Core ML program or neural network with inspectable operations"
        case .gpuSelected(let count):
            return "GPU was preferred for \(count) operations; this build permits no GPU fallback"
        case .insufficientANE(let actual, let required, let cpuPreferred):
            return String(
                format: "ANE residency %.2f%% is below required %.2f%% (%d CPU-preferred operations)",
                actual * 100,
                required * 100,
                cpuPreferred
            )
        }
    }
}
