import Darwin
import Foundation
import SwiftParser
import SwiftSyntax

struct FunctionMetric: Codable {
    let file: String
    let function: String
    let line: Int
    let endLine: Int
    let cyclomatic: Int

    enum CodingKeys: String, CodingKey {
        case file
        case function
        case line
        case endLine = "end_line"
        case cyclomatic
    }
}

final class ComplexityVisitor: SyntaxVisitor {
    private(set) var complexity = 1

    init() {
        super.init(viewMode: .sourceAccurate)
    }

    override func visit(_: IfExprSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_: GuardStmtSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_: ForStmtSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_: WhileStmtSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_: RepeatStmtSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_: CatchClauseSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_ node: SwitchCaseSyntax) -> SyntaxVisitorContinueKind {
        if let label = node.label.as(SwitchCaseLabelSyntax.self) {
            complexity += label.caseItems.count
        }
        return .visitChildren
    }

    override func visit(_: TernaryExprSyntax) -> SyntaxVisitorContinueKind {
        complexity += 1
        return .visitChildren
    }

    override func visit(_ node: BinaryOperatorExprSyntax) -> SyntaxVisitorContinueKind {
        let operation = node.operator.text
        if operation == "&&" || operation == "||" {
            complexity += 1
        }
        return .visitChildren
    }

    override func visit(_: FunctionDeclSyntax) -> SyntaxVisitorContinueKind {
        .skipChildren
    }

    override func visit(_: InitializerDeclSyntax) -> SyntaxVisitorContinueKind {
        .skipChildren
    }

    override func visit(_: DeinitializerDeclSyntax) -> SyntaxVisitorContinueKind {
        .skipChildren
    }

    override func visit(_: AccessorDeclSyntax) -> SyntaxVisitorContinueKind {
        .skipChildren
    }

    override func visit(_: ClosureExprSyntax) -> SyntaxVisitorContinueKind {
        .skipChildren
    }
}

final class FunctionCollector: SyntaxVisitor {
    private let file: String
    private let converter: SourceLocationConverter
    private(set) var metrics: [FunctionMetric] = []

    init(file: String, tree: SourceFileSyntax) {
        self.file = file
        converter = SourceLocationConverter(fileName: file, tree: tree)
        super.init(viewMode: .sourceAccurate)
    }

    private func append(
        name: String,
        node: some SyntaxProtocol,
        body: CodeBlockSyntax?
    ) {
        guard let body else { return }
        let visitor = ComplexityVisitor()
        visitor.walk(body)
        let start = converter.location(
            for: node.positionAfterSkippingLeadingTrivia
        ).line
        let end = converter.location(
            for: node.endPositionBeforeTrailingTrivia
        ).line
        metrics.append(
            FunctionMetric(
                file: file,
                function: name,
                line: start,
                endLine: end,
                cyclomatic: visitor.complexity
            )
        )
    }

    override func visit(_ node: FunctionDeclSyntax) -> SyntaxVisitorContinueKind {
        append(name: node.name.text, node: node, body: node.body)
        return .visitChildren
    }

    override func visit(_ node: InitializerDeclSyntax) -> SyntaxVisitorContinueKind {
        append(name: "init", node: node, body: node.body)
        return .visitChildren
    }

    override func visit(_ node: DeinitializerDeclSyntax) -> SyntaxVisitorContinueKind {
        append(name: "deinit", node: node, body: node.body)
        return .visitChildren
    }

    override func visit(_ node: AccessorDeclSyntax) -> SyntaxVisitorContinueKind {
        append(
            name: "<accessor:\(node.accessorSpecifier.text)>",
            node: node,
            body: node.body
        )
        return .visitChildren
    }

    override func visit(_ node: ClosureExprSyntax) -> SyntaxVisitorContinueKind {
        let location = converter.location(
            for: node.positionAfterSkippingLeadingTrivia
        )
        append(
            name: "<closure@\(location.line):\(location.column)>",
            node: node,
            body: CodeBlockSyntax(statements: node.statements)
        )
        return .visitChildren
    }
}

func analyze(path: String) throws -> [FunctionMetric] {
    let absolutePath = URL(fileURLWithPath: path).standardizedFileURL.path
    let source = try String(contentsOfFile: absolutePath, encoding: .utf8)
    let tree = Parser.parse(source: source)
    guard !tree.hasError else {
        throw NSError(
            domain: "swift-crap-analyzer",
            code: 2,
            userInfo: [NSLocalizedDescriptionKey: "Swift parse error: \(absolutePath)"]
        )
    }
    let collector = FunctionCollector(file: absolutePath, tree: tree)
    collector.walk(tree)
    return collector.metrics
}

do {
    let paths = Array(CommandLine.arguments.dropFirst())
    guard !paths.isEmpty else {
        throw NSError(
            domain: "swift-crap-analyzer",
            code: 2,
            userInfo: [NSLocalizedDescriptionKey: "at least one Swift source path is required"]
        )
    }
    let metrics = try paths.flatMap(analyze(path:))
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    let data = try encoder.encode(metrics)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
} catch {
    FileHandle.standardError.write(Data("Swift analysis error: \(error)\n".utf8))
    exit(2)
}
